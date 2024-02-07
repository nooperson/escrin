mod eth;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use ethers::types::Address;
use futures::stream::StreamExt as _;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, trace, warn};

pub use self::eth::{PermitRequestEvent, PermitRequestKind};
use crate::{
    store::Store,
    types::{ChainState, ChainStateUpdate, PermitterLocator},
    utils::{retry, retry_times},
    verify::Verifier as _,
};

#[tracing::instrument(skip_all)]
pub async fn run(
    store: impl Store + 'static,
    gateways: impl Iterator<Item = impl AsRef<str>>,
    permitter_addr: Address,
) -> Result<(), eth::Error> {
    trace!("collating providers");
    let providers = eth::providers(gateways).await?;

    for (chain, provider) in providers.into_iter() {
        let store = store.clone();
        let permitter = eth::SsssPermitter::new(chain, permitter_addr, provider);
        trace!("launching task for chain {chain}");
        tokio::spawn(async move {
            loop {
                match sync_chain(chain, &permitter, &store).await {
                    Ok(_) => warn!("sync task for chain {chain} unexpectedly exited"),
                    Err(e) => error!("sync task for chain {chain} exited with error: {e}"),
                }
                sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn sync_chain<S: Store + 'static>(
    chain_id: eth::ChainId,
    permitter: &eth::SsssPermitter,
    store: &S,
) -> Result<(), Error> {
    let start_block = match store.get_chain_state(chain_id).await? {
        Some(ChainState { block }) => block,
        None => permitter.creation_block().await?,
    };

    let processed_block = Arc::new(AtomicU64::new(start_block));
    let state_updater_task = tokio::spawn({
        let store = store.clone();
        let processed_block = processed_block.clone();
        async move {
            loop {
                sleep(Duration::from_secs(5 * 60)).await;
                debug!("updating sync state for chain {chain_id}");
                if let Err(e) = store
                    .update_chain_state(
                        chain_id,
                        ChainStateUpdate {
                            block: Some(processed_block.load(Ordering::Acquire)),
                        },
                    )
                    .await
                {
                    warn!("failed to update sync state for chain {chain_id}: {e}");
                }
            }
        }
    });

    let processed_block = &processed_block;
    permitter
        .events(start_block, None)
        .buffered(100)
        .map(futures::stream::iter)
        .flatten()
        .for_each(|event| async move {
            match event.kind {
                eth::EventKind::PermitRequest(req) => {
                    let policy_result = retry_times(
                        || {
                            store.get_verifier(
                                PermitterLocator::new(chain_id, permitter.address),
                                req.identity,
                            )
                        },
                        Some,
                        Some(3),
                    )
                    .await;
                    let policy = match policy_result {
                        Ok(p) => p.unwrap_or_default(),
                        Err(e) => {
                            error!("failed to fetch policy: {e}");
                            return;
                        }
                    };
                    let pass = match req.selector().as_deref() {
                        #[cfg(feature = "aws")]
                        Some("nitro") => {
                            crate::verify::NitroEnclaveVerifier
                                .verify(req, &policy)
                                .await
                        }
                        _ => {
                            warn!(
                                "encountered unknown context in: {}",
                                event.tx.unwrap_or_default()
                            );
                            None
                        }
                    };
                    if pass.is_none() {
                        return;
                    }
                    todo!()
                }
                eth::EventKind::Configuration(eth::ConfigurationEvent { identity, config }) => {
                    retry(|| {
                        store.update_verifier(
                            PermitterLocator::new(chain_id, permitter.address),
                            identity,
                            config.clone(),
                            event.index,
                        )
                    })
                    .await;
                }
                eth::EventKind::ProcessedBlock => {
                    processed_block.store(event.index.block, Ordering::Release);
                }
            }
        })
        .await;

    state_updater_task.abort();
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error(transparent)]
    Eth(#[from] eth::Error),
}
