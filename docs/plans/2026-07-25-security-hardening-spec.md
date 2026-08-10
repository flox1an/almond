# Security Hardening Specification

**Status:** Audit result; implementation not started  
**Datum:** 2026-07-25  
**Geltung:** HTTP-API, Blob-Speicher, Upstream-/Mirror-Pfade, Chunked Uploads, Authentisierung, Cashu und Deployment

## Ziel

Almond muss bei einer öffentlich erreichbaren Instanz weder durch einzelne HTTP-Anfragen
noch durch günstige Request-Schleifen in einen fehlerhaften, erschöpften oder destruktiven
Zustand gebracht werden können. Insbesondere dürfen unregistrierte Angreifer weder:

- RAM, Disk, Inodes, File Descriptors oder CPU unbegrenzt beanspruchen;
- interne Dienste über Mirror-/Upstream-Fetches erreichen;
- fremde Uploads blockieren oder destruktive Aktionen auslösen;
- eine aktivierte Zahlungsfunktion umgehen;
- sensible Betriebs- und lokale Metadaten aus Browser-Kontexten auslesen.

## Ausgangslage und Bedrohungsmodell

Die derzeitige Default-Konfiguration ist funktional öffentlich:

- `FEATURE_UPLOAD_ENABLED=public`;
- `FEATURE_MIRROR_ENABLED=public`;
- `ALLOWED_NPUBS` ist leer;
- das Docker-Image bindet an `0.0.0.0:3000`.

Im `public`-Modus ist eine gültige Nostr-Signatur erforderlich, aber jeder Angreifer kann
ein eigenes Schlüsselpaar erzeugen. In dieser Spezifikation bedeutet **nicht registrierter
Angreifer** deshalb: ein Client mit selbst erzeugtem, gültig signiertem Nostr-Event.

Die Befunde sind eine quellcodebasierte Analyse. Abhängig von Feature-Flags markierte
Befunde gelten nur, wenn das jeweilige Feature aktiviert wurde. Ein dynamischer
Penetrationstest gegen eine laufende Produktionsinstanz war nicht Teil dieser Analyse.

## Schutzinvarianten

1. Jede eingehende Anfrage hat eine transportseitig wirksame, endpointgerechte
   Body-Obergrenze. Streaming-Code erzwingt dieselbe Grenze selbst.
2. Temporäre Dateien und unvollständige Uploads sind vollständig quota-kontiert,
   begrenzt und auf allen Fehlerpfaden löschbar.
3. Kein URL-Fetch darf nach DNS-Auflösung oder Redirect ein privates, lokales,
   link-lokales oder anderweitig nicht öffentlich routbares Ziel erreichen.
4. Chunked Uploads gehören genau einem Pubkey; ein anderer Pubkey kann ihren
   Zustand weder verändern noch blockieren.
5. Öffentliche Read-/Discovery-Endpunkte haben eine begrenzte Rechen- und
   Speicherkomplexität pro Anfrage.
6. Destruktive Operationen benötigen mindestens dieselbe Autorisierungsstufe wie
   `DELETE /:filename`.
7. Aktivierte Bezahlpfade erlauben keine Arbeit oder Persistierung oberhalb der
   bezahlten Größe.
8. Infrastruktur- und Diagnoseendpunkte sind nicht unkontrolliert im öffentlichen
   Browser-Kontext lesbar.

## Priorität 0 — vor öffentlichem Betrieb beheben

### P0-1: Reales Request-Body-Limit erzwingen

**Befund:** `DefaultBodyLimit` in `src/main.rs:126-129` wirkt nur für Extractors, die
es anwenden. `upload_file`, `mirror_blob` und `patch_upload` nehmen dagegen
`Request<Body>` und konsumieren `req.into_body()` direkt (`src/handlers/upload.rs:101`,
`:177`, `:436`).

`stream_to_temp_file` (`src/services/upload.rs:184-235`) schreibt ohne eigene
Größenprüfung. Ein endloser `PUT /upload` kann deshalb die Partition über
`files/temp/upload_<uuid>` füllen. Die Hash-Autorisierung erfolgt erst nach dem
Streaming.

**Anforderung:**

- Einen transportseitig wirkenden `RequestBodyLimitLayer` einsetzen.
- Für `/mirror` ein separates, kleines Limit verwenden; der JSON-Body darf maximal
  64 KiB groß sein.
- `stream_to_temp_file` muss `max_bytes` erhalten, geschriebene Bytes zählen und
  beim Überschreiten mit `PayloadTooLarge` abbrechen.
- Eine maximale Gesamtblobgröße als Konfiguration einführen und im regulären sowie
  Chunked-Upload-Pfad erzwingen.
- Vor dem Schreiben eine konfigurierbare freie-Disk-Reserve prüfen; der Check ergänzt,
  ersetzt aber nicht die Byte-Grenze.

**Akzeptanzkriterien:**

- Ein Chunked-Transfer über dem Endpoint-Limit erhält `413`; der Temp-Dateipfad
  existiert danach nicht mehr.
- Eine unendliche oder sehr langsame Upload-Verbindung kann weder mehr als das Limit
  schreiben noch dauerhaft einen Temp-Dateideskriptor blockieren.
- Das Ergebnis gilt auch für Handler, die `Request<Body>` verwenden.

### P0-2: Unbegrenztes Mirror-Buffering entfernen

**Befund:** `mirror_blob` nutzt `axum::body::to_bytes(req.into_body(), usize::MAX)`
(`src/handlers/upload.rs:177-179`). Ein großer, Chunked Request wird vollständig in den
Heap gelesen, obwohl nur `{"url":"..."}` erwartet wird.

**Anforderung:**

- Den Mirror-Body auf 64 KiB begrenzen oder einen begrenzten `Json`-Extractor nutzen.
- Ein Überschreiten muss ein `413 Payload Too Large` sein.
- Fehlerdetails des Parsers dürfen nicht den vollständigen, fremdgesteuerten Body loggen.

**Akzeptanzkriterium:** Ein mehrgigabytegroßer bzw. endloser Body für `PUT /mirror`
belegt nicht mehr als das Endpoint-Limit im Heap und beendet den Prozess nicht.

### P0-3: SSRF über Redirects und Rebinding schließen

**Befund:** `validate_url_for_ssrf` validiert nur die initiale URL
(`src/services/upload.rs:53-135`). Die Clients folgen Redirects bis zu fünfmal
(`src/services/upload.rs:137-151`, `:153-181`), ohne das Redirect-Ziel erneut zu
validieren. Damit kann eine öffentliche HTTPS-Quelle auf ein internes Ziel umleiten.
Die initiale Auflösung und die spätere Client-Verbindung sind zudem nicht an dieselbe IP
gebunden.

**Anforderung:**

- Für alle SSRF-sensitiven Fetches Redirects standardmäßig deaktivieren.
- Falls Redirects benötigt werden: jeden Hop maximal einmal parsen, normalisieren,
  DNS-auflösen, gegen dieselbe Policy validieren und die Zahl der Hops klein begrenzen.
- Die verwendete Verbindung muss auf eine zuvor validierte öffentliche IP gepinnt sein;
  eine reine Vorabauflösung ist nicht ausreichend.
- Die IP-Policy muss IPv4, IPv6, IPv4-mapped IPv6, Loopback, Unspecified,
  Link-Local, RFC1918, Carrier-Grade NAT und weitere nicht öffentliche Reserven
  fail-closed behandeln.
- DNS-Timeouts dürfen nicht den gemeinsamen Tokio-Blocking-Pool erschöpfen.
  Einen asynchronen Resolver oder einen strikt separierten, begrenzten DNS-Pool einsetzen.

**Akzeptanzkriterien:**

- Eine öffentliche HTTPS-URL mit Redirect auf `127.0.0.1`, `::1`, eine IPv4-mapped
  Loopback-Adresse, `169.254.169.254` oder eine RFC1918-Adresse wird nicht angefragt.
- Ein Hostname, der nach erfolgreicher Validierung die IP wechselt, erreicht keine
  private Zieladresse.
- Viele absichtlich verzögerte DNS-Auflösungen blockieren keine Blob-Lese- oder
  Schreiboperationen.

### P0-4: Öffentlichen Report nicht destruktiv machen

**Befund:** Bei `FEATURE_REPORT_ENABLED=public` akzeptiert `PUT /report` jedes gültig
selbstsignierte Kind-1984-Event (`src/handlers/report.rs:112-127`). Mit
`REPORT_ACTION=delete` bzw. `quarantine` können beliebige bekannte Blob-Hashes gelöscht
oder unbrauchbar gemacht werden (`:196-224`).

**Anforderung:**

- `public` darf für Reports keine direkte Dateiaktion auslösen.
- Direkte Löschung verlangt mindestens die bestehende Strict-Whitelist-Autorisierung.
- Quarantäne durch Reports muss entweder ebenfalls streng autorisiert oder als
  moderationsbedürftige, nicht-destruktive Meldung persistiert werden.
- Report-Events benötigen eine kurze, geprüfte Gültigkeit und Replay-Schutz.

**Akzeptanzkriterium:** Ein selbstsigniertes Public-Report-Event kann keinen fremden
Blob löschen, verschieben oder aus dem Index entfernen.

## Priorität 1 — Verfügbarkeit und Integrität

### P1-1: Chunked Uploads begrenzen und an Eigentümer binden

**Befunde:**

- `X-SHA-256` wird in `PATCH /upload` nicht auf exakt 64 lowercase Hex-Zeichen
  geprüft (`src/handlers/upload.rs:327-330`). Der Wert fließt als Map-Key und in
  Chunk-Dateinamen (`:423-429`, `:479-492`).
- `chunk_uploads` ist allein durch den Hash indiziert (`src/models.rs:297`), nicht
  durch den authentisierten Pubkey.
- `Upload-Length` besitzt keine Obergrenze.
- `upload_offset + content_length` kann überlaufen (`src/handlers/upload.rs:405-411`).
- Die Map hat keine Kapazitätsgrenze. Leere Chunks verursachen dennoch Dateien und
  `sync_all()`.
- Der globale Write-Lock bleibt während eines Cashu-`await` gehalten
  (`src/handlers/upload.rs:479-561`).
- Parallele Abschlussrequests können denselben Upload gleichzeitig rekonstruieren;
  Fehlerpfade hinterlassen dabei `reconstruct_*`-Tempdateien.

**Anforderung:**

- Direkt nach dem Header-Parsing `file_storage::validate_sha256_format` anwenden und
  ausschließlich lowercase akzeptieren.
- Upload-Zustand mit `(PublicKey, sha256)` statt nur `sha256` indizieren.
- Maximale Anzahl paralleler Sessions global, pro Pubkey und pro Quell-IP festlegen.
- `Upload-Length` gegen die neue maximale Blobgröße prüfen.
- Für Offset plus Länge `checked_add` verwenden.
- Abschluss atomar beanspruchen: Upload-State unter Lock aus der Map entfernen oder in
  einen nicht erneut abschließbaren Zustand überführen, bevor Rekonstruktion beginnt.
- Kein Netzwerk-I/O unter gehaltenem `chunk_uploads`-Lock.
- Chunk-Dateien bei jedem Fehler löschen; Rekonstruktionsdateien per RAII-Guard
  sichern und den Cleanup auf `files/temp/` ausweiten.
- `sync_all()` nicht für jeden leeren oder winzigen Chunk erzwingen; ein definierter,
  dokumentierter Durability-Punkt genügt.

**Akzeptanzkriterien:**

- Ein anderer Pubkey kann einen Upload mit demselben Hash nicht blockieren oder
  verändern.
- Viele 0-Byte-PATCH-Requests führen weder zu unbegrenzten Map-Einträgen noch zu
  unkontrolliertem Inode-Verbrauch.
- Zwei parallele Abschlussversuche erzeugen genau eine Rekonstruktion und keine
  verbleibende `reconstruct_*`-Datei.
- Überlaufende Headerwerte liefern `400`, nie Panic oder State-Mutation.

### P1-2: Öffentliche Index- und Filter-Endpunkte skalierbar machen

**Befunde:**

- `/list` klont und sortiert den vollständigen Index vor der Pagination
  (`src/handlers/list.rs:263-296`, `src/services/blob_index.rs:131-140`).
- `/filter` akzeptiert beliebig viele `fp`-Werte als Cache-Key; bei Binary-Fuse ist
  der Wert nicht einmal ausgaberelevant, erzwingt aber Neubau.
- `/_wot` baut für jeden Request einen Bloom-Filter neu; `fp=NaN` führt zu einem
  Bloomfilter-Assert (`src/handlers/wot.rs:28-37`).
- `failed_upstream_lookups` ist eine stundenlang lebende, kapazitätslose Map
  (`src/handlers/file_serving.rs:330-336`, `src/utils.rs:437-444`).

**Anforderung:**

- `BlobIndex` muss eine paginierbare, nach `(created_at, sha256)` sortierte Sicht
  bieten, die höchstens die angeforderte Seitengröße klont.
- `/filter` muss `fp` für Binary-Fuse aus dem Cache-Key entfernen; Filterneubau
  single-flight ausführen und FP-Werte diskret begrenzen.
- `/_wot` muss nicht-endliche FP-Werte mit `400` ablehnen, nur feste FP-Stufen
  zulassen und Ergebnisse generationsbasiert cachen.
- Negative Lookups in einer kapazitätsbegrenzten LRU halten oder bei fehlenden
  Upstreams vollständig deaktivieren.
- Alle rechenintensiven öffentlichen Endpunkte erhalten Request- und Rate-Limits.

**Akzeptanzkriterien:**

- `GET /list?limit=1` allokiert nicht proportional zur gesamten Blobanzahl.
- Parallele Cache-Misses für denselben Filter erzeugen nur einen Filterbau.
- `GET /_wot?fp=NaN` liefert `400` ohne Panic-Log.
- Zufällige Hash-Requests vergrößern den Negativ-Cache nicht über die konfigurierte
  Kapazität.

### P1-3: Speicherquota korrekt und synchron durchsetzen

**Befund:** `enforce_storage_limits` sortiert nach Alter aufsteigend und behält zuerst die
ältesten Dateien (`src/utils.rs:158-205`). Bei gefülltem Speicher werden neue Uploads
zunächst mit Erfolg bestätigt und später gelöscht. Die Durchsetzung erfolgt nur periodisch;
Temporärdateien zählen nicht mit.

**Anforderung:**

- Das gewünschte Eviction-Modell explizit festlegen. Für FIFO-artige Aufbewahrung
  müssen die neuesten Objekte erhalten bleiben und die ältesten verdrängt werden.
- Alternativ Schreibvorgänge vor dem Persistieren mit `507 Insufficient Storage`
  ablehnen.
- Finalblobs, Chunk-Dateien, Rekonstruktionsdateien und HLS-Tempdateien müssen in
  der verfügbaren Disk-Reserve und Quota berücksichtigt werden.
- Die Prüfung muss neben dem periodischen Cleanup im Schreibpfad stattfinden.

**Akzeptanzkriterium:** Ein vollständig belegter Store bestätigt keinen Upload mit `201`,
der im nächsten Cleanup-Lauf wieder verschwindet.

### P1-4: HLS-Mirror begrenzen und aufräumen

**Befunde:**

- Fehlerpfade in `src/services/hls.rs` erzeugen Futures von `remove_file`, ohne sie
  zu awaiten; die Tempdateien bleiben erhalten.
- Playlist-Referenzen sind nicht global begrenzt; eine große Playlist kann sehr viele
  ausgehende Requests erzeugen.

**Anforderung:**

- Tempdateien per RAII oder garantiertem async Cleanup auf allen Fehlerpfaden löschen.
- Maximale Playlist-Größe, maximale Referenzen pro Runde, maximale Gesamtzahl von
  Referenzen und globale Deduplication festlegen.
- HLS-Fetches mit begrenzter Parallelität und Gesamtbudget ausführen.

## Priorität 2 — Authentisierung, Zahlung und Datenschutz

### P2-1: Upload-Whitelist tatsächlich erzwingen

**Befund:** `FeatureMode::Public` wird auf `AuthMode::Unrestricted` abgebildet. Ein
allein gesetztes `ALLOWED_NPUBS` begrenzt Uploads daher nicht.

**Anforderung:**

- Einen expliziten Whitelist-Modus einführen oder `public` bei nicht-leerem
  `ALLOWED_NPUBS` auf eine fail-closed Autorisierung abbilden.
- Dokumentation und Konfigurationsbeispiele müssen den tatsächlichen Modus erklären.

### P2-2: Tokens kurzlebig und replay-resistent machen

**Befund:** `verify_event` prüft Signatur, Vergangenheit und Ablauf, aber keine maximale
TTL, kein Höchstalter und keinen Replay-Cache (`src/services/auth.rs:47-81`). Fehlen
`server`-Tags, sind Tokens absichtlich serverübergreifend gültig.

**Anforderung:**

- Maximale Event-TTL und maximale Clock-Skew festlegen.
- Für destruktive Events mindestens einen TTL-basierten Event-ID-Replay-Cache führen.
- Eine Betreiberoption für verpflichtende Server-Bindung bereitstellen.
- BUD-11-Base64url ohne Padding akzeptieren; Standard-Base64 darf als kompatibler
  Fallback unterstützt werden.

### P2-3: Cashu-Pfade fail-closed ausführen

**Gilt nur bei aktivierten `FEATURE_PAID_*`-Flags.**

**Befunde:**

- Mirror-Bezahlung wird übersprungen, wenn der Upstream keine `Content-Length` liefert.
- Der Preis basiert auf der untrusted `Content-Length`, nicht auf den tatsächlich
  übertragenen Bytes.
- Der nach dem Swap tatsächlich gutgeschriebene Betrag wird nicht mit dem Preis
  verglichen.
- Der finale Chunk wird vor einem fehlgeschlagenen 402-Payment-Check gespeichert;
  ein Retry scheitert dann am Duplicate-Check.
- Der Wallet-Seed wird mit Standard-Dateirechten geschrieben.

**Anforderung:**

- Mirror-Payment anhand der tatsächlich gestreamten Bytezahl bestimmen oder fehlende
  Länge fail-closed behandeln.
- Netto eingelösten Betrag gegen den erforderlichen Betrag prüfen.
- Den Chunked-402-Pfad rollback-fähig machen.
- Wallet-Seed atomar mit `0600` anlegen.
- Entweder je akzeptierter Mint ein Wallet führen oder nur genau einen Mint
  konfigurieren und bewerben.
- Wallet-/Mint-Infrastrukturfehler als 5xx abbilden, nicht als Client-400.

### P2-4: Browser- und Betriebsmetadaten schützen

**Befunde:**

- `Access-Control-Allow-Origin: *` wird auf alle Endpunkte gesetzt
  (`src/middleware.rs:9-42`). Das erlaubt fremden Websites insbesondere bei lokalen
  Cache-Instanzen das Lesen von `/list`, `/_wot`, `/metrics` und Blobinhalten.
- `/metrics` und `/_metrics` sind öffentlich. Upstream-Hosts können bei aktivierten
  Custom Origins unbeschränkte Prometheus-Label erzeugen.

**Anforderung:**

- CORS-Wildcard auf öffentliche Blob-GET/HEAD-Antworten beschränken.
- Für Metadaten- und Diagnosepfade eine konfigurierte Origin-Allowlist verwenden.
- Metrics auf separatem Admin-Listener oder hinter Auth bereitstellen.
- Upstream-Metriklabels auf bekannte Hosts begrenzen oder unbekannte Ziele zu
  `other` zusammenfassen.

## Plattform- und Betriebsanforderungen

1. Eingehende HTTP-Verbindungen benötigen Header-Read-Timeout, Request-Timeout,
   globale Concurrency-Grenze und Per-IP-Rate-Limits.
2. Docker muss unter einem dedizierten Non-root-User laufen.
3. Das Runtime-Image muss von `debian:bullseye-slim` und `libssl1.1` auf eine
   unterstützte Basis migrieren.
4. Container-Dateisystem muss soweit möglich read-only sein; nur explizite Volumes für
   Blob- und Wallet-Daten sind beschreibbar.
5. `fips.pem` und `fips-key.pem` müssen in `.gitignore` ergänzt werden. Die Dateien
   sind derzeit nicht versioniert, aber nicht ausreichend gegen versehentlichen Commit
   geschützt.
6. `CLEANUP_INTERVAL_SECS=0` muss explizit abgelehnt oder sicher behandelt werden;
   ein Panic im Hintergrundjob darf Cleanup nicht dauerhaft beenden.
7. Hintergrundjobs müssen Fehler und Panics sichtbar machen sowie kontrolliert
   neu gestartet werden.
8. Dependency-Hygiene in CI etablieren: `cargo audit` und `cargo deny`. Der
   eingebundene Cashu/SQLite-Stack ist funktional, enthält aber eine alte gebündelte
   SQLite-Version und zusätzlich zwei Reqwest-Major-Versionen.

## Nicht-Befunde

Die Analyse fand keinen direkten Path-Traversal-Write über den finalen Blob-Pfad:
finale Pfade erhalten in den geprüften Pfaden nur Hashes, die gegen einen tatsächlich
berechneten SHA-256-Digest geprüft wurden. Die Chunk-Header-Validierung ist trotzdem
verpflichtend, weil die Rohwerte zuvor State und Temp-Dateinamen beeinflussen.

`DELETE /:filename` selbst ist fail-closed: Es verlangt eine nicht-leere Whitelist,
Strict Authorization sowie passende `t=delete`- und `x`-Tags. Der Report-Endpunkt darf
keinen schwächeren alternativen Löschpfad bieten.

## Abnahmeplan

Die Umsetzung ist erst abgeschlossen, wenn mindestens folgende Tests existieren und grün
sind:

1. Begrenzte und endlose Bodies für Upload und Mirror ergeben `413`; weder Disk noch
   Heap überschreiten das konfigurierte Budget.
2. SSRF-Redirects und DNS-Rebinding-Versuche erreichen keine nicht öffentlichen Ziele.
3. Fremde Pubkeys können Chunk-Sessions nicht blockieren; Sessions, Chunk-Dateien und
   Rekonstruktionsdateien bleiben innerhalb konfigurierter Limits.
4. Öffentliche `/list`, `/filter`, `/_wot` und Random-Hash-GETs bleiben unter
   Parallelität innerhalb eines vorgegebenen CPU-/Speicherbudgets.
5. Public Reports können keine Dateisystemaktion auslösen.
6. Mit aktivierten Paid-Features werden Chunked-, Mirror- und Download-Pfade nur nach
   korrekt eingelöster und ausreichender Zahlung abgeschlossen.
7. CORS- und Metrics-Tests bestätigen, dass lokale/administrative Metadaten nicht
   cross-origin bzw. öffentlich lesbar sind.
8. Der Container läuft non-root; der Wallet-Seed hat Modus `0600`; keine PEM-Datei
   außerhalb der vorgesehenen Secret-Verteilung wird versioniert.
