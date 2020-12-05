use crate::model::{Balances, GetTransactionsOpt, SPVVerifyResult};
use bitcoin::blockdata::script::Script;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{self, All, Message, Secp256k1};
use bitcoin::util::address::{Address, Payload};
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey, ExtendedPubKey};
use bitcoin::{BlockHash, PublicKey, SigHashType, Txid};
use elements;
use hex;
use log::{info, trace};
use rand::Rng;

use crate::model::{AddressPointer, CreateTransactionOpt, Settings, TransactionDetails, TXO};
use crate::network::{ElementsNetwork, Network, NetworkId};
use crate::scripts::{p2pkh_script, p2shwpkh_script, p2shwpkh_script_sig};
use bip39;
use wally::{
    asset_final_vbf, asset_generator_from_bytes, asset_rangeproof, asset_surjectionproof,
    asset_value_commitment, tx_get_elements_signature_hash,
};

use crate::error::{fn_err, Error};
use crate::store::{Store, StoreMeta};

use crate::be::{self, *};
use bitcoin::util::bip143::SigHashCache;
use electrum_client::raw_client::RawClient;
use electrum_client::Client;
use elements::confidential::{Asset, Nonce, Value};
use elements::slip77::MasterBlindingKey;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

pub struct WalletCtx {
    pub secp: Secp256k1<All>,
    pub network: Network,
    pub store: Store,
    pub xprv: ExtendedPrivKey,
    pub xpub: ExtendedPubKey,
    pub master_blinding: Option<MasterBlindingKey>,
    pub change_max_deriv: u32,
}

#[derive(Clone)]
pub enum ElectrumUrl {
    Tls(String, bool), // the bool value indicates if the domain name should be validated
    Plaintext(String),
}

impl ElectrumUrl {
    pub fn build_client(&self) -> Result<Client, Error> {
        match self {
            ElectrumUrl::Tls(url, validate) => {
                let client = RawClient::new_ssl(url.as_str(), *validate)?;
                Ok(Client::SSL(client))
            }
            ElectrumUrl::Plaintext(url) => {
                let client = RawClient::new(&url)?;
                Ok(Client::TCP(client))
            }
        }
    }
}

impl WalletCtx {
    pub fn from_mnemonic(mnemonic: &str, data_root: &str, network: Network) -> Result<Self, Error> {
        let mnemonic = bip39::Mnemonic::parse_in(bip39::Language::English, mnemonic)?;
        // TODO: passphrase?
        let passphrase = "".into();
        let seed = mnemonic.to_seed(passphrase);
        let secp = Secp256k1::new();
        let xprv =
            ExtendedPrivKey::new_master(bitcoin::network::constants::Network::Testnet, &seed)?;

        // BIP44: m / purpose' / coin_type' / account' / change / address_index
        // coin_type = 0 bitcoin, 1 testnet, 1776 liquid bitcoin as defined in https://github.com/satoshilabs/slips/blob/master/slip-0044.md
        // slip44 suggest 1 for every testnet, so we are using it also for regtest
        let coin_type: u32 = match network.id() {
            NetworkId::Bitcoin(bitcoin_network) => match bitcoin_network {
                bitcoin::Network::Bitcoin => 0,
                bitcoin::Network::Testnet => 1,
                bitcoin::Network::Regtest => 1,
            },
            NetworkId::Elements(elements_network) => match elements_network {
                ElementsNetwork::Liquid => 1776,
                ElementsNetwork::ElementsRegtest => 1,
            },
        };
        // since we use P2WPKH-nested-in-P2SH it is 49 https://github.com/bitcoin/bips/blob/master/bip-0049.mediawiki
        let path_string = format!("m/49'/{}'/0'", coin_type);
        info!("Using derivation path {}/0|1/*", path_string);
        let path = DerivationPath::from_str(&path_string)?;
        let xprv = xprv.derive_priv(&secp, &path)?;
        let xpub = ExtendedPubKey::from_private(&secp, &xprv);

        let wallet_desc = format!("{}{:?}", xpub, network);
        let wallet_id = hex::encode(sha256::Hash::hash(wallet_desc.as_bytes()));

        let master_blinding = if network.liquid {
            Some(MasterBlindingKey::new(&seed))
        } else {
            None
        };

        let mut path: PathBuf = data_root.into();
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }
        path.push(wallet_id);
        info!("Store root path: {:?}", path);
        let store = Arc::new(RwLock::new(StoreMeta::new(
            &path,
            xpub,
            master_blinding.clone(),
            network.id(),
        )?));

        Ok(WalletCtx {
            store,
            network, // TODO: from db
            secp,
            xprv,
            xpub,
            master_blinding,
            change_max_deriv: 0,
        })
    }

    fn get_master_blinding_key(&self) -> &MasterBlindingKey {
        self.master_blinding
            .as_ref()
            .expect("we are in elements but master blinding is None")
    }

    fn derive_address(&self, xpub: &ExtendedPubKey, path: [u32; 2]) -> Result<BEAddress, Error> {
        let path: Vec<ChildNumber> = path
            .iter()
            .map(|x| ChildNumber::Normal { index: *x })
            .collect();
        let derived = xpub.derive_pub(&self.secp, &path)?;
        match self.network.id() {
            NetworkId::Bitcoin(network) => Ok(BEAddress::Bitcoin(
                Address::p2shwpkh(&derived.public_key, network).unwrap(),
            )),
            NetworkId::Elements(network) => {
                let script = p2shwpkh_script(&derived.public_key);
                let blinding_key = self.get_master_blinding_key().derive_blinding_key(&script);
                let public_key = secp256k1::PublicKey::from_secret_key(&self.secp, &blinding_key);
                let blinder = Some(public_key);
                let addr = elements::Address::p2shwpkh(
                    &derived.public_key,
                    blinder,
                    address_params(network),
                );

                Ok(BEAddress::Elements(addr))
            }
        }
    }

    pub fn get_settings(&self) -> Result<Settings, Error> {
        Ok(self.store.read()?.get_settings().unwrap_or_default())
    }

    pub fn change_settings(&self, settings: &Settings) -> Result<(), Error> {
        self.store
            .write()?
            .insert_settings(Some(settings.clone()))?;
        Ok(())
    }

    pub fn get_tip(&self) -> Result<(u32, BlockHash), Error> {
        Ok(self.store.read()?.cache.tip)
    }

    pub fn list_tx(&self, opt: &GetTransactionsOpt) -> Result<Vec<TransactionDetails>, Error> {
        let store_read = self.store.read()?;

        let mut txs = vec![];
        let mut my_txids: Vec<(&Txid, &Option<u32>)> = store_read.cache.heights.iter().collect();
        my_txids.sort_by(|a, b| {
            let height_cmp =
                b.1.unwrap_or(std::u32::MAX)
                    .cmp(&a.1.unwrap_or(std::u32::MAX));
            match height_cmp {
                Ordering::Equal => b.0.cmp(a.0),
                h @ _ => h,
            }
        });

        for (tx_id, height) in my_txids.iter().skip(opt.first).take(opt.count) {
            trace!("tx_id {}", tx_id);

            let tx = store_read
                .cache
                .all_txs
                .get(*tx_id)
                .ok_or_else(fn_err(&format!("list_tx no tx {}", tx_id)))?;

            let fee = tx.fee(
                &store_read.cache.all_txs,
                &store_read.cache.unblinded,
                &self.network.policy_asset().ok(),
            )?;
            trace!("tx_id {} fee {}", tx_id, fee);

            let balances = tx.my_balance_changes(
                &store_read.cache.all_txs,
                &store_read.cache.paths,
                &store_read.cache.unblinded,
            );
            trace!("tx_id {} balances {:?}", tx_id, balances);

            let spv_verified = if self.network.spv_enabled.unwrap_or(false) {
                store_read
                    .cache
                    .txs_verif
                    .get(*tx_id)
                    .unwrap_or(&SPVVerifyResult::InProgress)
                    .clone()
            } else {
                SPVVerifyResult::Disabled
            };

            trace!("tx_id {} spv_verified {:?}", tx_id, spv_verified);

            let tx_details =
                TransactionDetails::new(tx.clone(), balances, fee, **height, spv_verified);

            txs.push(tx_details);
        }
        info!(
            "list_tx {:?}",
            txs.iter().map(|e| &e.txid).collect::<Vec<&String>>()
        );

        Ok(txs)
    }

    pub fn utxos(&self) -> Result<Vec<TXO>, Error> {
        info!("start utxos");

        let store_read = self.store.read()?;
        let mut txos = vec![];
        let spent = store_read.spent()?;
        for (tx_id, height) in store_read.cache.heights.iter() {
            let tx = store_read
                .cache
                .all_txs
                .get(tx_id)
                .ok_or_else(fn_err(&format!("txos no tx {}", tx_id)))?;
            let tx_txos: Vec<TXO> = match tx {
                BETransaction::Bitcoin(tx) => tx
                    .output
                    .clone()
                    .into_iter()
                    .enumerate()
                    .filter(|(_, output)| output.value > DUST_VALUE)
                    .map(|(vout, output)| (BEOutPoint::new_bitcoin(tx.txid(), vout as u32), output))
                    .filter_map(|(vout, output)| {
                        store_read
                            .cache
                            .paths
                            .get(&output.script_pubkey)
                            .map(|path| (vout, output, path))
                    })
                    .filter(|(outpoint, _, _)| !spent.contains(&outpoint))
                    .map(|(outpoint, output, path)| {
                        TXO::new(
                            outpoint,
                            "btc".to_string(),
                            output.value,
                            None,
                            None,
                            output.script_pubkey,
                            height.clone(),
                            path.clone(),
                        )
                    })
                    .collect(),
                BETransaction::Elements(tx) => {
                    let policy_asset = self.network.policy_asset_id()?;
                    tx.output
                        .clone()
                        .into_iter()
                        .enumerate()
                        .map(|(vout, output)| {
                            (BEOutPoint::new_elements(tx.txid(), vout as u32), output)
                        })
                        .filter_map(|(vout, output)| {
                            store_read
                                .cache
                                .paths
                                .get(&output.script_pubkey)
                                .map(|path| (vout, output, path))
                        })
                        .filter(|(outpoint, _, _)| !spent.contains(&outpoint))
                        .filter_map(|(outpoint, output, path)| {
                            if let BEOutPoint::Elements(el_outpoint) = outpoint {
                                if let Some(unblinded) =
                                    store_read.cache.unblinded.get(&el_outpoint)
                                {
                                    if unblinded.value < DUST_VALUE
                                        && unblinded.asset == policy_asset
                                    {
                                        return None;
                                    }
                                    return Some(TXO::new(
                                        outpoint,
                                        unblinded.asset_hex(),
                                        unblinded.value,
                                        None,
                                        None,
                                        output.script_pubkey,
                                        height.clone(),
                                        path.clone(),
                                    ));
                                }
                            }
                            None
                        })
                        .collect()
                }
            };
            txos.extend(tx_txos);
        }
        txos.sort_by(|a, b| b.satoshi.cmp(&a.satoshi));

        Ok(txos)
    }

    pub fn balance(&self) -> Result<Balances, Error> {
        info!("start balance");
        let mut result = HashMap::new();
        match self.network.id() {
            NetworkId::Bitcoin(_) => result.entry("btc".to_string()).or_insert(0),
            NetworkId::Elements(_) => result
                .entry(self.network.policy_asset.as_ref().unwrap().clone())
                .or_insert(0),
        };
        for u in self.utxos()?.iter() {
            *result.entry(u.asset.clone()).or_default() += u.satoshi as i64;
        }
        Ok(result)
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn create_tx(&self, opt: &mut CreateTransactionOpt) -> Result<TransactionDetails, Error> {
        info!("create_tx {:?}", opt);

        // TODO put checks into CreateTransaction::validate, add check asset_tag are valid asset hex
        // eagerly check for address validity
        for address in opt.addressees.iter().map(|a| &a.address) {
            match self.network.id() {
                NetworkId::Bitcoin(network) => {
                    if let Ok(address) = bitcoin::Address::from_str(address) {
                        info!("address.network:{} network:{}", address.network, network);
                        if address.network == network
                            || (address.network == bitcoin::Network::Testnet
                                && network == bitcoin::Network::Regtest)
                        {
                            continue;
                        }
                        if let Payload::WitnessProgram {
                            version: v,
                            program: _p,
                        } = &address.payload
                        {
                            // Do not support segwit greater than v0
                            if v.to_u8() > 0 {
                                return Err(Error::InvalidAddress);
                            }
                        }
                    }
                    return Err(Error::InvalidAddress);
                }
                NetworkId::Elements(network) => {
                    if let Ok(address) = elements::Address::from_str(address) {
                        info!(
                            "address.params:{:?} address_params(network):{:?}",
                            address.params,
                            address_params(network)
                        );
                        if address.params == address_params(network) {
                            continue;
                        }
                    }
                    return Err(Error::InvalidAddress);
                }
            }
        }

        if opt.addressees.is_empty() {
            return Err(Error::EmptyAddressees);
        }

        let send_all = opt.send_all.unwrap_or(false);
        opt.send_all = Some(send_all); // accept default false, but always return the value
        if !send_all && opt.addressees.iter().any(|a| a.satoshi == 0) {
            return Err(Error::InvalidAmount);
        }

        if !send_all {
            for address_amount in opt.addressees.iter() {
                if address_amount.satoshi <= be::DUST_VALUE {
                    match self.network.id() {
                        NetworkId::Bitcoin(_) => return Err(Error::InvalidAmount),
                        NetworkId::Elements(_) => {
                            if address_amount.asset_tag == self.network.policy_asset {
                                // we apply dust rules for liquid bitcoin as elements do
                                return Err(Error::InvalidAmount);
                            }
                        }
                    }
                }
            }
        }

        if let NetworkId::Elements(_) = self.network.id() {
            if opt.addressees.iter().any(|a| a.asset_tag.is_none()) {
                return Err(Error::AssetEmpty);
            }
        }

        // convert from satoshi/kbyte to satoshi/byte
        let default_value = match self.network.id() {
            NetworkId::Bitcoin(_) => 1000,
            NetworkId::Elements(_) => 100,
        };
        let fee_rate = (opt.fee_rate.unwrap_or(default_value) as f64) / 1000.0;
        info!("target fee_rate {:?} satoshi/byte", fee_rate);

        let utxos = match &opt.utxos {
            None => self.utxos()?,
            Some(utxos) => utxos.clone(),
        };
        info!("utxos len:{}", utxos.len());

        if send_all {
            // send_all works by creating a dummy tx with all utxos, estimate the fee and set the
            // sending amount to `total_amount_utxos - estimated_fee`
            info!("send_all calculating total_amount");
            if opt.addressees.len() != 1 {
                return Err(Error::SendAll);
            }
            // FIXME: this error prone...
            let asset = opt.addressees[0].asset_tag.as_deref().unwrap_or("btc");
            let all_utxos: Vec<&TXO> = utxos.iter().filter(|u| u.asset == asset).collect();
            let total_amount_utxos: u64 = all_utxos.iter().map(|u| u.satoshi).sum();

            let to_send = if asset == "btc" || Some(asset.to_string()) == self.network.policy_asset
            {
                let mut dummy_tx = BETransaction::new(self.network.id());
                for utxo in all_utxos.iter() {
                    dummy_tx.add_input(utxo.outpoint.clone());
                }
                let out = &opt.addressees[0]; // safe because we checked we have exactly one recipient
                dummy_tx
                    .add_output(&out.address, out.satoshi, out.asset_tag.clone())
                    .map_err(|_| Error::InvalidAddress)?;
                let estimated_fee = dummy_tx.estimated_fee(fee_rate, 0) + 3; // estimating 3 satoshi more as estimating less would later result in InsufficientFunds
                total_amount_utxos
                    .checked_sub(estimated_fee)
                    .ok_or_else(|| Error::InsufficientFunds)?
            } else {
                total_amount_utxos
            };

            info!("send_all asset: {} to_send:{}", asset, to_send);

            opt.addressees[0].satoshi = to_send;
        }

        let mut tx = BETransaction::new(self.network.id());
        // transaction is created in 3 steps:
        // 1) adding requested outputs to tx outputs
        // 2) adding enough utxso to inputs such that tx outputs and estimated fees are covered
        // 3) adding change(s)

        // STEP 1) add the outputs requested for this transactions
        for out in opt.addressees.iter() {
            tx.add_output(&out.address, out.satoshi, out.asset_tag.clone())
                .map_err(|_| Error::InvalidAddress)?;
        }

        // STEP 2) add utxos until tx outputs are covered (including fees) or fail
        let store_read = self.store.read()?;
        let mut used_utxo: HashSet<BEOutPoint> = HashSet::new();
        loop {
            let mut needs = tx.needs(
                fee_rate,
                send_all,
                self.network.policy_asset.clone(),
                &store_read.cache.all_txs,
                &store_read.cache.unblinded,
            ); // Vec<(asset_string, satoshi)  "policy asset" is last, in bitcoin asset_string="btc" and max 1 element
            info!("needs: {:?}", needs);
            if needs.is_empty() {
                // SUCCESS tx doesn't need other inputs
                break;
            }
            let current_need = needs.pop().unwrap(); // safe to unwrap just checked it's not empty

            // taking only utxos of current asset considered, filters also utxos used in this loop
            let mut asset_utxos: Vec<&TXO> = utxos
                .iter()
                .filter(|u| u.asset == current_need.asset && !used_utxo.contains(&u.outpoint))
                .collect();

            // sort by biggest utxo, random maybe another option, but it should be deterministically random (purely random breaks send_all algorithm)
            asset_utxos.sort_by(|a, b| a.satoshi.cmp(&b.satoshi));
            let utxo = asset_utxos.pop().ok_or(Error::InsufficientFunds)?;

            match self.network.id() {
                NetworkId::Bitcoin(_) => {
                    // UTXO with same script must be spent together
                    for other_utxo in utxos.iter() {
                        if other_utxo.script_pubkey == utxo.script_pubkey {
                            used_utxo.insert(other_utxo.outpoint.clone());
                            tx.add_input(other_utxo.outpoint.clone());
                        }
                    }
                }
                NetworkId::Elements(_) => {
                    // Don't spend same script together in liquid. This would allow an attacker
                    // to cheaply send assets without value to the target, which will have to
                    // waste fees for the extra tx inputs and (eventually) outputs.
                    // While blinded address are required and not public knowledge,
                    // they are still available to whom transacted with us in the past
                    used_utxo.insert(utxo.outpoint.clone());
                    tx.add_input(utxo.outpoint.clone());
                }
            }
        }

        // STEP 3) adding change(s)
        let estimated_fee = tx.estimated_fee(
            fee_rate,
            tx.estimated_changes(
                send_all,
                &store_read.cache.all_txs,
                &store_read.cache.unblinded,
            ),
        );
        let changes = tx.changes(
            estimated_fee,
            self.network.policy_asset.clone(),
            &store_read.cache.all_txs,
            &store_read.cache.unblinded,
        ); // Vec<Change> asset, value
        for (i, change) in changes.iter().enumerate() {
            let change_index = store_read.cache.indexes.internal + i as u32 + 1;
            let change_address = self
                .derive_address(&self.xpub, [1, change_index])?
                .to_string();
            info!(
                "adding change to {} of {} asset {:?}",
                &change_address, change.satoshi, change.asset
            );
            tx.add_output(&change_address, change.satoshi, Some(change.asset.clone()))?;
        }

        // randomize inputs and outputs, BIP69 has been rejected because lacks wallets adoption
        tx.scramble();

        let policy_asset = self.network.policy_asset().ok();
        let fee_val = tx.fee(
            &store_read.cache.all_txs,
            &store_read.cache.unblinded,
            &policy_asset,
        )?; // recompute exact fee_val from built tx
        tx.add_fee_if_elements(fee_val, &policy_asset)?;

        info!("created tx fee {:?}", fee_val);

        let mut satoshi = tx.my_balance_changes(
            &store_read.cache.all_txs,
            &store_read.cache.paths,
            &store_read.cache.unblinded,
        );

        for (_, v) in satoshi.iter_mut() {
            *v = v.abs();
        }

        // Also return changes used?
        Ok(TransactionDetails::new(
            tx,
            satoshi,
            fee_val,
            None,
            SPVVerifyResult::NotVerified,
        ))
    }
    // TODO when we can serialize psbt
    //pub fn sign(&self, psbt: PartiallySignedTransaction) -> Result<PartiallySignedTransaction, Error> { Err(Error::Generic("NotImplemented".to_string())) }

    fn internal_sign_bitcoin(
        &self,
        tx: &Transaction,
        input_index: usize,
        path: &DerivationPath,
        value: u64,
    ) -> (Script, Vec<Vec<u8>>) {
        let xprv = self.xprv.derive_priv(&self.secp, &path).unwrap();
        let private_key = &xprv.private_key;
        let public_key = &PublicKey::from_private_key(&self.secp, private_key);
        let witness_script = p2pkh_script(public_key);

        let hash = SigHashCache::new(tx).signature_hash(
            input_index,
            &witness_script,
            value,
            SigHashType::All,
        );

        let message = Message::from_slice(&hash.into_inner()[..]).unwrap();
        let signature = self.secp.sign(&message, &private_key.key);

        let mut signature = signature.serialize_der().to_vec();
        signature.push(SigHashType::All as u8);

        let script_sig = p2shwpkh_script_sig(public_key);
        let witness = vec![signature, public_key.to_bytes()];
        info!(
            "added size len: script_sig:{} witness:{}",
            script_sig.len(),
            witness.iter().map(|v| v.len()).sum::<usize>()
        );

        (script_sig, witness)
    }

    pub fn internal_sign_elements(
        &self,
        tx: &elements::Transaction,
        input_index: usize,
        derivation_path: &DerivationPath,
        value: Value,
    ) -> (Script, Vec<Vec<u8>>) {
        let xprv = self.xprv.derive_priv(&self.secp, &derivation_path).unwrap();
        let private_key = &xprv.private_key;
        let public_key = &PublicKey::from_private_key(&self.secp, private_key);

        let script_code = p2pkh_script(public_key);
        let sighash = tx_get_elements_signature_hash(
            &tx,
            input_index,
            &script_code,
            &value,
            SigHashType::All.as_u32(),
            true, // segwit
        );
        let message = secp256k1::Message::from_slice(&sighash[..]).unwrap();
        let signature = self.secp.sign(&message, &private_key.key);
        let mut signature = signature.serialize_der().to_vec();
        signature.push(SigHashType::All as u8);

        let script_sig = p2shwpkh_script_sig(public_key);
        let witness = vec![signature, public_key.to_bytes()];
        info!(
            "added size len: script_sig:{} witness:{}",
            script_sig.len(),
            witness.iter().map(|v| v.len()).sum::<usize>()
        );
        (script_sig, witness)
    }

    pub fn sign(&self, be_tx: &mut BETransaction) -> Result<(), Error> {
        info!("sign");
        let store_read = self.store.read()?;
        match be_tx {
            BETransaction::Bitcoin(tx) => {
                for i in 0..tx.input.len() {
                    let prev_output = tx.input[i].previous_output;
                    info!("input#{} prev_output:{:?}", i, prev_output);
                    let prev_tx = store_read.get_bitcoin_tx(&prev_output.txid)?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path: DerivationPath = store_read
                        .cache
                        .paths
                        .get(&out.script_pubkey)
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .clone();
                    info!(
                        "input#{} prev_output:{:?} derivation_path:{:?}",
                        i, prev_output, derivation_path
                    );

                    let (script_sig, witness) =
                        self.internal_sign_bitcoin(&tx, i, &derivation_path, out.value);

                    tx.input[i].script_sig = script_sig;
                    tx.input[i].witness = witness;
                }
                info!(
                    "transaction final size is {} bytes and {} vbytes",
                    tx.get_size(),
                    tx.get_weight() / 4
                );
                info!(
                    "FINALTX inputs:{} outputs:{}",
                    tx.input.len(),
                    tx.output.len()
                );
            }
            BETransaction::Elements(tx) => {
                // FIXME: is blinding here the right thing to do?
                self.blind_tx(tx)?;

                for i in 0..tx.input.len() {
                    let prev_output = tx.input[i].previous_output;
                    info!("input#{} prev_output:{:?}", i, prev_output);
                    let prev_tx = store_read.get_liquid_tx(&prev_output.txid)?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path: DerivationPath = store_read
                        .cache
                        .paths
                        .get(&out.script_pubkey)
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .clone();

                    let (script_sig, witness) =
                        self.internal_sign_elements(&tx, i, &derivation_path, out.value);

                    tx.input[i].script_sig = script_sig;
                    tx.input[i].witness.script_witness = witness;
                }

                let fee: u64 = tx
                    .output
                    .iter()
                    .filter(|o| o.is_fee())
                    .map(|o| o.minimum_value())
                    .sum();
                info!(
                    "transaction final size is {} bytes and {} vbytes and fee is {}",
                    tx.get_size(),
                    tx.get_weight() / 4,
                    fee
                );
                info!(
                    "FINALTX inputs:{} outputs:{}",
                    tx.input.len(),
                    tx.output.len()
                );
            }
        };

        /*
        drop(store_read);
        let mut store_write = self.store.write()?;

        let changes_used = request.changes_used.unwrap_or(0);
        if changes_used > 0 {
            info!("tx used {} changes", changes_used);
            // The next sync would update the internal index but we increment the internal index also
            // here after sign so that if we immediately create another tx we are not reusing addresses
            // This implies signing multiple times without broadcasting leads to gaps in the internal chain
            store_write.cache.indexes.internal += changes_used;
        }
        */

        Ok(())
    }

    fn blind_tx(&self, tx: &mut elements::Transaction) -> Result<(), Error> {
        info!("blind_tx {}", tx.txid());
        let mut input_assets = vec![];
        let mut input_abfs = vec![];
        let mut input_vbfs = vec![];
        let mut input_ags = vec![];
        let mut input_values = vec![];
        let store_read = self.store.read()?;
        for input in tx.input.iter() {
            info!("input {:?}", input);

            let unblinded = store_read
                .cache
                .unblinded
                .get(&input.previous_output)
                .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?;
            info!(
                "unblinded value: {} asset:{}",
                unblinded.value,
                hex::encode(&unblinded.asset[..])
            );

            input_values.push(unblinded.value);
            input_assets.extend(unblinded.asset.to_vec());
            input_abfs.extend(unblinded.abf.to_vec());
            input_vbfs.extend(unblinded.vbf.to_vec());
            let input_asset = asset_generator_from_bytes(&unblinded.asset, &unblinded.abf);
            input_ags.extend(elements::encode::serialize(&input_asset));
        }

        let ct_exp = self.network.ct_exponent.unwrap_or(0);
        let ct_bits = self.network.ct_bits.unwrap_or(52);
        info!("ct params ct_exp:{}, ct_bits:{}", ct_exp, ct_bits);

        let mut output_blinded_values = vec![];
        for output in tx.output.iter() {
            if !output.is_fee() {
                output_blinded_values.push(output.minimum_value());
            }
        }
        info!("output_blinded_values {:?}", output_blinded_values);
        let mut all_values = vec![];
        all_values.extend(input_values);
        all_values.extend(output_blinded_values);
        let in_num = tx.input.len();
        let out_num = tx.output.len();

        let output_abfs: Vec<Vec<u8>> = (0..out_num - 1).map(|_| random32()).collect();
        let mut output_vbfs: Vec<Vec<u8>> = (0..out_num - 2).map(|_| random32()).collect();

        let mut all_abfs = vec![];
        all_abfs.extend(input_abfs.to_vec());
        all_abfs.extend(output_abfs.iter().cloned().flatten().collect::<Vec<u8>>());

        let mut all_vbfs = vec![];
        all_vbfs.extend(input_vbfs.to_vec());
        all_vbfs.extend(output_vbfs.iter().cloned().flatten().collect::<Vec<u8>>());

        let last_vbf = asset_final_vbf(all_values, in_num, all_abfs, all_vbfs);
        output_vbfs.push(last_vbf.to_vec());

        for (i, mut output) in tx.output.iter_mut().enumerate() {
            info!("output {:?}", output);
            if !output.is_fee() {
                match (output.value, output.asset, output.nonce) {
                    (Value::Explicit(value), Asset::Explicit(asset), Nonce::Confidential(_, _)) => {
                        info!("value: {}", value);
                        let nonce = elements::encode::serialize(&output.nonce);
                        let blinding_pubkey = PublicKey::from_slice(&nonce).unwrap();
                        let blinding_key = self
                            .get_master_blinding_key()
                            .derive_blinding_key(&output.script_pubkey);
                        let blinding_public_key =
                            secp256k1::PublicKey::from_secret_key(&self.secp, &blinding_key);
                        let mut output_abf = [0u8; 32];
                        output_abf.copy_from_slice(&(&output_abfs[i])[..]);
                        let mut output_vbf = [0u8; 32];
                        output_vbf.copy_from_slice(&(&output_vbfs[i])[..]);
                        let asset = asset.clone().into_inner();

                        let output_generator =
                            asset_generator_from_bytes(&asset.into_inner(), &output_abf);
                        let output_value_commitment =
                            asset_value_commitment(value, output_vbf, output_generator);
                        let min_value = if output.script_pubkey.is_provably_unspendable() {
                            0
                        } else {
                            1
                        };

                        let rangeproof = asset_rangeproof(
                            value,
                            blinding_pubkey.key,
                            blinding_key,
                            asset.into_inner(),
                            output_abf,
                            output_vbf,
                            output_value_commitment,
                            &output.script_pubkey,
                            output_generator,
                            min_value,
                            ct_exp,
                            ct_bits,
                        );
                        trace!("asset: {}", hex::encode(&asset));
                        trace!("output_abf: {}", hex::encode(&output_abf));
                        trace!(
                            "output_generator: {}",
                            hex::encode(&elements::encode::serialize(&output_generator))
                        );
                        trace!("input_assets: {}", hex::encode(&input_assets));
                        trace!("input_abfs: {}", hex::encode(&input_abfs));
                        trace!("input_ags: {}", hex::encode(&input_ags));
                        trace!("in_num: {}", in_num);

                        let surjectionproof = asset_surjectionproof(
                            asset.into_inner(),
                            output_abf,
                            output_generator,
                            output_abf,
                            &input_assets,
                            &input_abfs,
                            &input_ags,
                            in_num,
                        );
                        trace!("surjectionproof: {}", hex::encode(&surjectionproof));

                        let bytes = blinding_public_key.serialize();
                        let byte32: [u8; 32] = bytes[1..].as_ref().try_into().unwrap();
                        output.nonce =
                            elements::confidential::Nonce::Confidential(bytes[0], byte32);
                        output.asset = output_generator;
                        output.value = output_value_commitment;
                        info!(
                            "added size len: surjectionproof:{} rangeproof:{}",
                            surjectionproof.len(),
                            rangeproof.len()
                        );
                        output.witness.surjection_proof = surjectionproof;
                        output.witness.rangeproof = rangeproof;
                    }
                    _ => panic!("create_tx created things not right"),
                }
            }
        }
        Ok(())
    }

    pub fn get_address(&self) -> Result<AddressPointer, Error> {
        let pointer = {
            let store = &mut self.store.write()?.cache;
            store.indexes.external += 1;
            store.indexes.external
        };
        let address = self.derive_address(&self.xpub, [0, pointer])?.to_string();
        Ok(AddressPointer { address, pointer })
    }
}

fn address_params(net: ElementsNetwork) -> &'static elements::AddressParams {
    match net {
        ElementsNetwork::Liquid => &elements::AddressParams::LIQUID,
        ElementsNetwork::ElementsRegtest => &elements::AddressParams::ELEMENTS,
    }
}

fn random32() -> Vec<u8> {
    rand::thread_rng().gen::<[u8; 32]>().to_vec()
}

#[cfg(test)]
mod test {
    use crate::interface::p2shwpkh_script_sig;
    use bitcoin::blockdata::transaction::SigHashType;
    use bitcoin::consensus::deserialize;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{All, Message, Secp256k1, SecretKey};
    use bitcoin::util::bip143::SigHashCache;
    use bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey};
    use bitcoin::util::key::PrivateKey;
    use bitcoin::util::key::PublicKey;
    use bitcoin::Script;
    use bitcoin::{Address, Network, Transaction};
    use std::str::FromStr;

    fn p2pkh_hex(pk: &str) -> (PublicKey, Script) {
        let pk = hex::decode(pk).unwrap();
        let pk = PublicKey::from_slice(pk.as_slice()).unwrap();
        let witness_script = Address::p2pkh(&pk, Network::Bitcoin).script_pubkey();
        (pk, witness_script)
    }

    #[test]
    fn test_bip() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();

        // https://github.com/bitcoin/bips/blob/master/bip-0143.mediawiki#p2sh-p2wpkh
        let tx_bytes = hex::decode("0100000001db6b1b20aa0fd7b23880be2ecbd4a98130974cf4748fb66092ac4d3ceb1a54770100000000feffffff02b8b4eb0b000000001976a914a457b684d7f0d539a46a45bbc043f35b59d0d96388ac0008af2f000000001976a914fd270b1ee6abcaea97fea7ad0402e8bd8ad6d77c88ac92040000").unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let private_key_bytes =
            hex::decode("eb696a065ef48a2192da5b28b694f87544b30fae8327c4510137a922f32c6dcf")
                .unwrap();

        let key = SecretKey::from_slice(&private_key_bytes).unwrap();
        let private_key = PrivateKey {
            compressed: true,
            network: Network::Testnet,
            key,
        };

        let (public_key, witness_script) =
            p2pkh_hex("03ad1d8e89212f0b92c74d23bb710c00662ad1470198ac48c43f7d6f93a2a26873");
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a91479091972186c449eb1ded22b78e40d009bdf008988ac"
        );
        let value = 1_000_000_000;
        let hash = SigHashCache::new(&tx)
            .signature_hash(0, &witness_script, value, SigHashType::All)
            .into_inner();

        assert_eq!(
            &hash[..],
            &hex::decode("64f3b0f4dd2bb3aa1ce8566d220cc74dda9df97d8490cc81d89d735c92e59fb6")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        assert_eq!(signature_hex, "3044022047ac8e878352d3ebbde1c94ce3a10d057c24175747116f8288e5d794d12d482f0220217f36a485cae903c713331d877c1f64677e3622ad4010726870540656fe9dcb01");

        let script_sig = p2shwpkh_script_sig(&public_key);

        assert_eq!(
            format!("{}", hex::encode(script_sig.as_bytes())),
            "16001479091972186c449eb1ded22b78e40d009bdf0089"
        );
    }

    #[test]
    fn test_my_tx() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();
        let xprv = ExtendedPrivKey::from_str("tprv8jdzkeuCYeH5hi8k2JuZXJWV8sPNK62ashYyUVD9Euv5CPVr2xUbRFEM4yJBB1yBHZuRKWLeWuzH4ptmvSgjLj81AvPc9JhV4i8wEfZYfPb").unwrap();
        let xpub = ExtendedPubKey::from_private(&secp, &xprv);
        let private_key = xprv.private_key;
        let public_key = xpub.public_key;
        let public_key_bytes = public_key.to_bytes();
        let public_key_str = format!("{}", hex::encode(&public_key_bytes));

        let address = Address::p2shwpkh(&public_key, Network::Testnet).unwrap();
        assert_eq!(
            format!("{}", address),
            "2NCEMwNagVAbbQWNfu7M7DNGxkknVTzhooC"
        );

        assert_eq!(
            public_key_str,
            "0386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc"
        );
        let tx_hex = "020000000001010e73b361dd0f0320a33fd4c820b0c7ac0cae3b593f9da0f0509cc35de62932eb01000000171600141790ee5e7710a06ce4a9250c8677c1ec2843844f0000000002881300000000000017a914cc07bc6d554c684ea2b4af200d6d988cefed316e87a61300000000000017a914fda7018c5ee5148b71a767524a22ae5d1afad9a9870247304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01210386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc00000000";

        let tx_bytes = hex::decode(tx_hex).unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let (_, witness_script) = p2pkh_hex(&public_key_str);
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a9141790ee5e7710a06ce4a9250c8677c1ec2843844f88ac"
        );
        let value = 10_202;
        let hash =
            SigHashCache::new(&tx).signature_hash(0, &witness_script, value, SigHashType::All);

        assert_eq!(
            &hash.into_inner()[..],
            &hex::decode("58b15613fc1701b2562430f861cdc5803531d08908df531082cf1828cd0b8995")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        let signature = hex::decode(&signature_hex).unwrap();

        assert_eq!(signature_hex, "304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01");
        assert_eq!(tx.input[0].witness[0], signature);
        assert_eq!(tx.input[0].witness[1], public_key_bytes);

        let script_sig = p2shwpkh_script_sig(&public_key);
        assert_eq!(tx.input[0].script_sig, script_sig);
    }
}
