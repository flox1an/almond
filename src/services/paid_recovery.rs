use crate::{
    error::AppResult,
    models::AppState,
    services::{cashu, file_storage, paid_purchase, paid_store::OpState, upload},
};

/// Reconcile recorded purchases without ever treating a spent input alone as proof of receipt.
pub async fn recover(state: &AppState) -> AppResult<()> {
    let Some(store) = &state.paid_store else {
        return Ok(());
    };
    let now = paid_purchase::now();
    for operation in store.unresolved().await? {
        let terms = &operation.terms;
        if operation.state == OpState::Pending {
            if let Some(wallet) = &state.cashu_wallet {
                if let Some(amount) = cashu::find_payment(wallet, &terms.op_id).await? {
                    if amount >= terms.amount {
                        store
                            .mark_settled(&terms.op_id, Some(terms.op_id.clone()), now)
                            .await?;
                    }
                }
            }
        }
        let Some(updated) = store.operation(&terms.op_id).await? else {
            continue;
        };
        if updated.state != OpState::Settled {
            continue;
        }
        if state.file_index.get(&terms.sha256).await.is_none() {
            let stage = state
                .storage
                .root
                .join("paid-pending")
                .join(format!("{}_{}", terms.op_id, terms.sha256));
            if tokio::fs::try_exists(&stage)
                .await
                .map_err(|e| crate::error::AppError::IoError(e.to_string()))?
            {
                use sha2::{Digest, Sha256};
                use tokio::io::AsyncReadExt;
                let mut file = tokio::fs::File::open(&stage)
                    .await
                    .map_err(|e| crate::error::AppError::IoError(e.to_string()))?;
                let mut hasher = Sha256::new();
                let mut buffer = vec![0u8; 65536];
                let mut size = 0u64;
                loop {
                    let count = file
                        .read(&mut buffer)
                        .await
                        .map_err(|e| crate::error::AppError::IoError(e.to_string()))?;
                    if count == 0 {
                        break;
                    }
                    size = size.saturating_add(count as u64);
                    hasher.update(&buffer[..count]);
                }
                if size != terms.size || hex::encode(hasher.finalize()) != terms.sha256 {
                    return Err(crate::error::AppError::Conflict(
                        "Staged blob does not match purchase".to_owned(),
                    ));
                }
                let temp = file_storage::TempBlob::reserve(state, "paid-recover", None);
                tokio::fs::hard_link(&stage, temp.path())
                    .await
                    .map_err(|e| {
                        crate::error::AppError::IoError(format!(
                            "Unable to recover staged blob: {e}"
                        ))
                    })?;
                upload::finalize_upload(state, temp, &terms.sha256, terms.size, None, None, None)
                    .await?;
            }
        }
        if state.file_index.get(&terms.sha256).await.is_some() {
            file_storage::apply_paid_operation(state, &terms.op_id, now).await?;
            let stage = state
                .storage
                .root
                .join("paid-pending")
                .join(format!("{}_{}", terms.op_id, terms.sha256));
            match tokio::fs::remove_file(stage).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(crate::error::AppError::IoError(format!(
                        "Unable to clear staged upload: {error}"
                    )))
                }
            }
        }
    }
    Ok(())
}
