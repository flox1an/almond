# Binary and systemd

Almond always runs in the foreground, with no daemon mode, so run it under a
service manager. Build and install the binary:

```bash
cargo build --release
sudo install -m 0755 target/release/almond /usr/local/bin/
```

The unit file [`contrib/almond.service`](https://github.com/flox1an/almond/blob/main/contrib/almond.service)
includes its install instructions in the header:

<<< @/../contrib/almond.service{ini}

Settings go into `/etc/almond/almond.env`, a dotenv file with the
[environment variables](/configuration/environment).
