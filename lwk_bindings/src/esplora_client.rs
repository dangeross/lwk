use std::sync::{Arc, Mutex};

use lwk_wollet::BlockchainBackend;

use crate::{LwkError, Transaction, Txid, Update, Wollet};

/// Wrapper over [`lwk_wollet::EsploraClient`]
#[derive(uniffi::Object, Debug)]
pub struct EsploraClient {
    inner: Mutex<lwk_wollet::EsploraClient>,
}

#[uniffi::export]
impl EsploraClient {
    /// Construct an Esplora Client
    #[uniffi::constructor]
    pub fn new(url: &str) -> Arc<Self> {
        let client = lwk_wollet::EsploraClient::new(url);
        Arc::new(Self {
            inner: Mutex::new(client),
        })
    }

    /// Construct an Esplora Client using Waterfalls endpoint
    #[uniffi::constructor]
    pub fn new_waterfalls(url: &str) -> Arc<Self> {
        let client = lwk_wollet::EsploraClient::new_waterfalls(url);
        Arc::new(Self {
            inner: Mutex::new(client),
        })
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<Arc<Txid>, LwkError> {
        Ok(Arc::new(self.inner.lock()?.broadcast(tx.as_ref())?.into()))
    }

    pub fn full_scan(&self, wollet: &Wollet) -> Result<Option<Arc<Update>>, LwkError> {
        self.full_scan_to_index(wollet, 0)
    }

    pub fn full_scan_to_index(
        &self,
        wollet: &Wollet,
        index: u32,
    ) -> Result<Option<Arc<Update>>, LwkError> {
        let wollet = wollet.inner_wollet()?;
        let update: Option<lwk_wollet::Update> = self
            .inner
            .lock()?
            .full_scan_to_index(&wollet.state(), index)?;
        Ok(update.map(Into::into).map(Arc::new))
    }
}
