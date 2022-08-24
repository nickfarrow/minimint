use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;

use std::hash::Hasher;
use std::sync::Arc;

use crate::bitcoind::BitcoindRpc;
use crate::config::WalletConfig;
use crate::db::{
    BlockHashKey, PegOutTxNonceCI, PegOutTxSignatureCI, PegOutTxSignatureCIPrefix,
    PendingTransactionKey, PendingTransactionPrefixKey, RoundConsensusKey, UTXOKey, UTXOPrefixKey,
    UnsignedTransactionKey, UnsignedTransactionPrefixKey,
};
use std::hash::Hash;

use crate::tweakable::Tweakable;
use crate::txoproof::{PegInProof, PegInProofError};
use async_trait::async_trait;
use bitcoin::hashes::{sha256, Hash as BitcoinHash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::{All, Secp256k1};
use bitcoin::util::psbt::raw::ProprietaryKey;
use bitcoin::util::psbt::{Input, PartiallySignedTransaction};
use bitcoin::util::sighash::SighashCache;
use bitcoin::util::taproot;
use bitcoin::{
    Address, AddressType, Amount, BlockHash, Network, SchnorrSighashType, Script, Transaction,
    TxIn, TxOut, Txid,
};
use db::PegOutTxNonceCIPrefix;
use frost::{FrostNonce, FrostSigShare};
use minimint_api::db::batch::BatchItem;
use minimint_api::db::batch::BatchTx;
use minimint_api::db::Database;
use minimint_api::encoding::{Decodable, Encodable};
use minimint_api::module::api_endpoint;
use minimint_api::module::interconnect::ModuleInterconect;
use minimint_api::module::ApiEndpoint;
use minimint_api::{FederationModule, InputMeta, OutPoint, PeerId};
use minimint_derive::UnzipConsensus;
use miniscript::psbt::PsbtExt;
use miniscript::{Descriptor, DescriptorTrait, TranslatePk3};
use rand::{CryptoRng, Rng, RngCore};
use serde::{Deserialize, Serialize};
use std::ops::Sub;

use minimint_api::module::audit::Audit;
use minimint_api::task::sleep;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, instrument, trace, warn};

pub mod bitcoind;
pub mod config;
pub mod db;
pub mod frost;
pub mod keys;
pub mod tweakable;
pub mod txoproof;

#[cfg(feature = "native")]
pub mod bitcoincore_rpc;

pub const CONFIRMATION_TARGET: u16 = 10;

pub type PartialSig = Vec<u8>;

pub type PegInDescriptor = Descriptor<secp256k1::PublicKey>;

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, UnzipConsensus, Encodable, Decodable,
)]
pub enum WalletConsensusItem {
    RoundConsensus(RoundConsensusItem),
    PegOutNonce(PegOutNonceItem),
    PegOutSignature(PegOutSignatureItem),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct RoundConsensusItem {
    pub block_height: u32, // FIXME: use block hash instead, but needs more complicated verification logic
    pub fee_rate: Feerate,
    pub randomness: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct PegOutSignatureItem {
    pub txid: Txid,
    // Change to signature shares of FROST
    pub signatures: Vec<FrostSigShare>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash, Deserialize, Encodable, Decodable)]
pub struct PegOutNonceItem {
    pub txid: Txid,
    pub nonces: Vec<FrostNonce>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct RoundConsensus {
    pub block_height: u32,
    pub fee_rate: Feerate,
    pub randomness_beacon: [u8; 32],
}

pub struct Wallet {
    cfg: WalletConfig,
    secp: Secp256k1<All>,
    btc_rpc: Box<dyn BitcoindRpc>,
    db: Arc<dyn Database>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct SpendableUTXO {
    pub tweak: [u8; 32],
    #[serde(with = "bitcoin::util::amount::serde::as_sat")]
    pub amount: bitcoin::Amount,
}

/// A peg-out tx that is ready to be broadcast with a tweak for the change UTXO
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct PendingTransaction {
    pub tx: Transaction,
    pub tweak: [u8; 32],
    pub change: bitcoin::Amount,
}

/// A PSBT that is awaiting enough signatures from the federation to becoming a `PendingTransaction`
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct UnsignedTransaction {
    pub psbt: PartiallySignedTransaction,
    pub nonces: Vec<(PeerId, PegOutNonceItem)>,
    pub signatures: Vec<(PeerId, PegOutSignatureItem)>,
    pub change: bitcoin::Amount,
    pub fees: PegOutFees,
}

struct StatelessWallet<'a> {
    descriptor: &'a Descriptor<secp256k1::PublicKey>,
    secp: &'a secp256k1::Secp256k1<secp256k1::All>,
}

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Ord,
    PartialOrd,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Encodable,
    Decodable,
)]
pub struct Feerate {
    pub sats_per_kvb: u64,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct PegOutFees {
    pub fee_rate: Feerate,
    pub total_weight: u64,
}

impl PegOutFees {
    pub fn amount(&self) -> Amount {
        self.fee_rate.calculate_fee(self.total_weight)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct PegOut {
    pub recipient: bitcoin::Address,
    #[serde(with = "bitcoin::util::amount::serde::as_sat")]
    pub amount: bitcoin::Amount,
    pub fees: PegOutFees,
}

#[async_trait(?Send)]
impl FederationModule for Wallet {
    type Error = WalletError;
    type TxInput = Box<PegInProof>;
    type TxOutput = PegOut;
    // TODO: implement outcome
    type TxOutputOutcome = ();
    type ConsensusItem = WalletConsensusItem;
    type VerificationCache = ();

    async fn await_consensus_proposal<'a>(&'a self, rng: impl RngCore + CryptoRng + 'a) {
        let mut our_target_height = self.target_height().await;
        let last_consensus_height = self.consensus_height().unwrap_or(0);

        if self.consensus_proposal(rng).await.len() == 1 {
            while our_target_height <= last_consensus_height {
                our_target_height = self.target_height().await;
                sleep(Duration::from_millis(1000)).await;
            }
        }
    }

    async fn consensus_proposal<'a>(
        &'a self,
        mut rng: impl RngCore + CryptoRng + 'a,
    ) -> Vec<Self::ConsensusItem> {
        // TODO: implement retry logic in case bitcoind is temporarily unreachable
        let our_target_height = self.target_height().await;

        // In case the wallet just got created the height is not committed to the DB yet but will
        // be set to 0 first, so we can assume that here.
        let last_consensus_height = self.consensus_height().unwrap_or(0);

        let proposed_height = if our_target_height >= last_consensus_height {
            our_target_height
        } else {
            warn!(
                "The block height shrunk, new proposal would be {}, but we are sticking to the last consensus height {}.",
                our_target_height,
                last_consensus_height
            );
            last_consensus_height
        };

        let fee_rate = self
            .btc_rpc
            .get_fee_rate(CONFIRMATION_TARGET)
            .await
            .unwrap_or(self.cfg.default_fee);

        let round_ci = WalletConsensusItem::RoundConsensus(RoundConsensusItem {
            block_height: proposed_height,
            fee_rate,
            randomness: rng.gen(),
        });

        let nonce_proposals = self.db.find_by_prefix(&PegOutTxNonceCIPrefix).map(|res| {
            let (key, nonces) = res.expect("FB error");
            WalletConsensusItem::PegOutNonce(PegOutNonceItem {
                txid: key.0,
                nonces,
            })
        });

        let signature_proposals = self
            .db
            .find_by_prefix(&PegOutTxSignatureCIPrefix)
            .map(|res| {
                let (key, signatures) = res.expect("FB error");
                WalletConsensusItem::PegOutSignature(PegOutSignatureItem {
                    txid: key.0,
                    signatures,
                })
            });

        signature_proposals
            .chain(nonce_proposals)
            .chain(std::iter::once(round_ci))
            .collect()
    }

    async fn begin_consensus_epoch<'a>(
        &'a self,
        mut batch: BatchTx<'a>,
        consensus_items: Vec<(PeerId, Self::ConsensusItem)>,
        _rng: impl RngCore + CryptoRng + 'a,
    ) {
        trace!(?consensus_items, "Received consensus proposals");

        // Separate round consensus items from signatures for peg-out tx. While signatures can be
        // processed separately, all round consensus items need to be available at once.
        let UnzipWalletConsensusItem {
            peg_out_nonce,
            peg_out_signature: peg_out_signatures,
            round_consensus,
        } = consensus_items.into_iter().unzip_wallet_consensus_item();

        // Save nonces and signatures to the database
        self.save_peg_out_signatures(batch.subtransaction(), peg_out_nonce, peg_out_signatures);

        // FIXME: also warn on less than 1/3, that should never happen
        // Make sure we have enough contributions to continue
        if round_consensus.is_empty() {
            panic!("No proposals were submitted this round");
        }

        let fee_proposals = round_consensus.iter().map(|(_, rc)| rc.fee_rate).collect();
        let fee_rate = self.process_fee_proposals(fee_proposals).await;

        let height_proposals = round_consensus
            .iter()
            .map(|(_, rc)| rc.block_height)
            .collect();
        let block_height = self
            .process_block_height_proposals(batch.subtransaction(), height_proposals)
            .await;

        let randomness_contributions = round_consensus
            .iter()
            .map(|(_, rc)| rc.randomness)
            .collect();
        let randomness_beacon = self.process_randomness_contributions(randomness_contributions);

        let round_consensus = RoundConsensus {
            block_height,
            fee_rate,
            randomness_beacon,
        };

        batch.append_insert(RoundConsensusKey, round_consensus);
        batch.commit();
    }

    fn build_verification_cache<'a>(
        &'a self,
        _inputs: impl Iterator<Item = &'a Self::TxInput>,
    ) -> Self::VerificationCache {
    }

    fn validate_input<'a>(
        &self,
        _interconnect: &dyn ModuleInterconect,
        _cache: &Self::VerificationCache,
        input: &'a Self::TxInput,
    ) -> Result<InputMeta<'a>, Self::Error> {
        if !self.block_is_known(input.proof_block()) {
            return Err(WalletError::UnknownPegInProofBlock(input.proof_block()));
        }

        // input.verify(&self.secp, &self.cfg.peg_in_descriptor)?;

        if self
            .db
            .get_value(&UTXOKey(input.outpoint()))
            .expect("DB error")
            .is_some()
        {
            return Err(WalletError::PegInAlreadyClaimed);
        }

        Ok(InputMeta {
            amount: minimint_api::Amount::from_sat(input.tx_output().value),
            puk_keys: Box::new(std::iter::once(*input.tweak_contract_key())),
        })
    }

    fn apply_input<'a, 'b>(
        &'a self,
        interconnect: &'a dyn ModuleInterconect,
        mut batch: BatchTx<'a>,
        input: &'b Self::TxInput,
        cache: &Self::VerificationCache,
    ) -> Result<InputMeta<'b>, Self::Error> {
        let meta = self.validate_input(interconnect, cache, input)?;
        debug!(outpoint = %input.outpoint(), amount = %meta.amount, "Claiming peg-in");

        batch.append_insert_new(
            UTXOKey(input.outpoint()),
            SpendableUTXO {
                tweak: input.tweak_contract_key().serialize(),
                amount: bitcoin::Amount::from_sat(input.tx_output().value),
            },
        );

        batch.commit();
        Ok(meta)
    }

    fn validate_output(
        &self,
        output: &Self::TxOutput,
    ) -> Result<minimint_api::Amount, Self::Error> {
        if !is_address_valid_for_network(&output.recipient, self.cfg.network) {
            return Err(WalletError::WrongNetwork(
                self.cfg.network,
                output.recipient.network,
            ));
        }
        let consensus_fee_rate = self.current_round_consensus().unwrap().fee_rate;
        if output.fees.fee_rate < consensus_fee_rate {
            return Err(WalletError::PegOutFeeRate(
                output.fees.fee_rate,
                consensus_fee_rate,
            ));
        }
        if self.create_peg_out_tx(output).is_none() {
            return Err(WalletError::NotEnoughSpendableUTXO);
        }
        Ok(output.amount.into())
    }

    fn apply_output<'a>(
        &'a self,
        mut batch: BatchTx<'a>,
        output: &'a Self::TxOutput,
        _out_point: minimint_api::OutPoint,
    ) -> Result<minimint_api::Amount, Self::Error> {
        let amount = self.validate_output(output)?;
        debug!(
            amount = %output.amount, recipient = %output.recipient,
            "Queuing peg-out",
        );

        let tx = self
            .create_peg_out_tx(output)
            .expect("Should have been validated");
        let txid = tx.psbt.unsigned_tx.txid();
        info!(
            %txid,
            "generating nonces for peg out",
        );

        // Delete used UTXOs
        batch.append_from_iter(
            tx.psbt
                .unsigned_tx
                .input
                .iter()
                .map(|input| BatchItem::delete(UTXOKey(input.previous_output))),
        );

        let frost_instance = frost::new_frost();
        let nonces = tx
            .psbt
            .inputs
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let sid = [(i as u32).to_be_bytes().as_slice(), &txid[..]].concat();
                // TODO MAKE SURE UNIQUE/NONREUSED
                frost::FrostNonce(
                    frost_instance
                        .gen_nonce(
                            &self.cfg.peg_in_key,
                            &sid,
                            // Some(self.cfg.frost_key.public_key().mark::<Normal>()),
                            None,
                            None,
                        )
                        .public,
                )
            })
            .collect::<Vec<_>>();
        batch.append_insert_new(UnsignedTransactionKey(txid), tx);
        batch.append_insert_new(PegOutTxNonceCI(txid), nonces);
        batch.commit();
        Ok(amount)
    }

    async fn end_consensus_epoch<'a>(
        &'a self,
        consensus_peers: &HashSet<PeerId>,
        mut batch: BatchTx<'a>,
        _rng: impl RngCore + CryptoRng + 'a,
    ) -> Vec<PeerId> {
        let txs_with_nonces = self
            .db
            .find_by_prefix(&UnsignedTransactionPrefixKey)
            .map(|res| res.expect("DB error"))
            .filter(|(_, unsigned)| !unsigned.nonces.is_empty() && unsigned.signatures.is_empty())
            .collect::<HashMap<_, _>>();

        let txs_with_signature_shares = self
            .db
            .find_by_prefix(&UnsignedTransactionPrefixKey)
            .map(|res| res.expect("DB error"))
            .filter(|(_, unsigned)| !unsigned.signatures.is_empty())
            .collect::<HashMap<_, _>>();

        let mut drop_peers = HashSet::<PeerId>::new();

        for (_, tx) in &txs_with_nonces {
            let peers_who_provided_nonces = tx
                .nonces
                .iter()
                .map(|(peer, _)| *peer) // TODO check nonces have the right length ()
                .collect::<HashSet<_>>();

            for peer in consensus_peers.sub(&peers_who_provided_nonces) {
                error!(
                    "Dropping {:?} for not contributing frost nonces to signing session",
                    peer
                );
                drop_peers.insert(peer);
            }
        }

        for (txid, tx) in &txs_with_signature_shares {
            for input_index in 0..tx.psbt.inputs.len() {
                let frost_instance = frost::new_frost();
                let (sign_session, frost_key, _) = self.create_sign_session(&tx, input_index);
                let consensus_peers = consensus_peers
                    .iter()
                    .map(|peer_id| peer_id.to_usize() as u32)
                    .collect::<HashSet<_>>();
                let peers_who_promised_sigs = sign_session
                    .participants()
                    .collect::<HashSet<_>>()
                    .intersection(&consensus_peers)
                    .cloned()
                    .collect::<HashSet<_>>();
                let peers_who_provided_sigs = sign_session
                    .participants()
                    .filter_map(|peer| {
                        let signature_shares = &tx
                            .signatures
                            .iter()
                            .find(|(peer_id, _)| peer_id.to_usize() == peer as usize)?
                            .1;
                        let signature_share = signature_shares.signatures.get(input_index)?.0;

                        if frost_instance.verify_signature_share(
                            &frost_key,
                            &sign_session,
                            peer,
                            signature_share,
                        ) {
                            Some(peer)
                        } else {
                            warn!(
                                "peer {} provided an invalid signature share on input {} in {}",
                                peer, input_index, txid.0
                            );
                            None
                        }
                    })
                    .collect::<HashSet<_>>();

                for peer in peers_who_promised_sigs.sub(&peers_who_provided_sigs) {
                    error!(
                        "Dropping {:?} for not contributing FROST signature shares",
                        peer
                    );
                    drop_peers.insert(PeerId::from(peer as u16));
                }
            }
        }

        for (txid, mut tx) in txs_with_nonces {
            if txs_with_signature_shares.contains_key(&txid) {
                continue;
            }
            let mut tx_secret_shares = vec![];
            let ordered_nonces: BTreeMap<_, _> = tx
                .nonces
                .iter()
                .filter(|(peer, _)| {
                    if drop_peers.contains(peer) {
                        warn!(
                            "not using FROST nonce from peer {} because we're dropping him",
                            peer
                        );
                        false
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();

            tx.nonces = ordered_nonces.into_iter().collect();

            if tx.nonces.len() >= self.cfg.frost_key.threshold() as usize {
                let frost_instance = frost::new_frost();
                for (input_index, _) in tx.psbt.inputs.iter().enumerate() {
                    let (sign_session, on_chain_frost_key, _) =
                        self.create_sign_session(&tx, input_index);
                    let sid = [(input_index as u32).to_be_bytes().as_slice(), &txid.0[..]].concat();
                    let nonce_kp =
                        frost_instance.gen_nonce(&self.cfg.peg_in_key, &sid[..], None, None);
                    if sign_session
                        .participants()
                        .find(|peer| self.cfg.peer_id.to_usize() == *peer as usize)
                        .is_some()
                    {
                        let signature_share_for_input = frost_instance.sign(
                            &on_chain_frost_key,
                            &sign_session,
                            self.cfg.peer_id.to_usize() as u32,
                            &self.cfg.peg_in_key,
                            nonce_kp,
                        );
                        tx_secret_shares.push(frost::FrostSigShare(signature_share_for_input));
                    } else {
                        warn!(
                            "peer {} decided to not contribute sig share because they were dropped",
                            self.cfg.peer_id
                        );
                    }
                }

                // Guessing that we can insert without delete
                batch.append_insert(txid.clone(), tx);
                batch.append_delete(PegOutTxNonceCI(txid.0));
                batch.append_insert(PegOutTxSignatureCI(txid.0), tx_secret_shares);
            } else {
                warn!("unable to start signing phase of frost signature because we don't have enough nonces")
            }
        }

        for (txid, tx) in txs_with_signature_shares {
            let mut success = true;
            let mut pending_tx = tx.psbt.clone().extract_tx();
            let frost_instance = frost::new_frost();

            for input_index in 0..pending_tx.input.len() {
                let (sign_session, on_chain_frost_key, message) =
                    self.create_sign_session(&tx, input_index);

                let sig_shares = sign_session
                    .participants()
                    .map(|peer| {
                        Some(
                            tx.signatures
                                .iter()
                                .find(|(peer_id, _)| peer_id.to_usize() == peer as usize)?
                                .1
                                .signatures
                                .get(input_index)?
                                .0,
                        )
                    })
                    .collect::<Option<Vec<_>>>();

                match sig_shares {
                    Some(sig_shares) => {
                        let signature = frost_instance.combine_signature_shares(
                            &on_chain_frost_key,
                            &sign_session,
                            sig_shares,
                        );

                        assert!(frost_instance.schnorr.verify(
                            &on_chain_frost_key.public_key(),
                            frost::Message::<schnorr_fun::fun::marker::Public>::raw(&message[..]),
                            &signature
                        ));
                        pending_tx.input[input_index]
                            .witness
                            .push(signature.to_bytes())
                    }
                    None => {
                        info!(
                            "missing shares from participants for input {} on {} so waiting for more",
                            input_index,
                            pending_tx.txid()
                        );
                        success = false;
                        continue;
                    }
                }
            }

            if success {
                let change_tweak: [u8; 32] = tx
                    .psbt
                    .outputs
                    .iter()
                    .flat_map(|output| output.proprietary.get(&proprietary_tweak_key()).cloned())
                    .next()
                    .unwrap()
                    .try_into()
                    .unwrap();

                batch.append_insert_new(
                    PendingTransactionKey(txid.0),
                    PendingTransaction {
                        tx: pending_tx,
                        tweak: change_tweak,
                        change: tx.change,
                    },
                );
                batch.append_delete(PegOutTxSignatureCI(txid.0));
                batch.append_delete(txid);
            }
        }

        batch.commit();
        drop_peers.into_iter().collect()
    }

    fn output_status(&self, _out_point: OutPoint) -> Option<Self::TxOutputOutcome> {
        // TODO: return BTC tx id once included in peg-out tx
        Some(())
    }

    fn audit(&self, audit: &mut Audit) {
        audit.add_items(&self.db, &UTXOPrefixKey, |_, v| {
            v.amount.as_sat() as i64 * 1000
        });
        audit.add_items(&self.db, &UnsignedTransactionPrefixKey, |_, v| {
            v.change.as_sat() as i64 * 1000
        });
        audit.add_items(&self.db, &PendingTransactionPrefixKey, |_, v| {
            v.change.as_sat() as i64 * 1000
        });
    }

    fn api_base_name(&self) -> &'static str {
        "wallet"
    }

    fn api_endpoints(&self) -> &'static [ApiEndpoint<Self>] {
        const ENDPOINTS: &[ApiEndpoint<Wallet>] = &[
            api_endpoint! {
                "/block_height",
                async |module: &Wallet, _params: ()| -> u32 {
                    Ok(module.consensus_height().unwrap_or(0))
                }
            },
            api_endpoint! {
                "/peg_out_fees",
                async |module: &Wallet, params: (Address, u64)| -> Option<PegOutFees> {
                    let (address, sats) = params;
                    let consensus = module.current_round_consensus().unwrap();
                    let tx = module.offline_wallet().create_tx(
                        bitcoin::Amount::from_sat(sats),
                        address.script_pubkey(),
                        module.available_utxos(),
                        consensus.fee_rate,
                        &consensus.randomness_beacon
                    );

                    Ok(tx.map(|tx| tx.fees))
                }
            },
        ];
        ENDPOINTS
    }
}

impl Wallet {
    // TODO: work around bitcoind_gen being a closure, maybe make clonable?
    pub async fn new_with_bitcoind(
        cfg: WalletConfig,
        db: Arc<dyn Database>,
        bitcoind_gen: impl Fn() -> Box<dyn BitcoindRpc>,
    ) -> Result<Wallet, WalletError> {
        let broadcaster_bitcoind_rpc = bitcoind_gen();
        let broadcaster_db = db.clone();
        minimint_api::task::spawn(async move {
            run_broadcast_pending_tx(broadcaster_db, broadcaster_bitcoind_rpc).await;
        });

        let bitcoind_rpc = bitcoind_gen();

        let bitcoind_net = bitcoind_rpc.get_network().await;
        if bitcoind_net != cfg.network {
            return Err(WalletError::WrongNetwork(cfg.network, bitcoind_net));
        }

        let wallet = Wallet {
            cfg,
            secp: Default::default(),
            btc_rpc: bitcoind_rpc,
            db,
        };

        Ok(wallet)
    }

    pub fn process_randomness_contributions(&self, randomness: Vec<[u8; 32]>) -> [u8; 32] {
        fn xor(mut lhs: [u8; 32], rhs: [u8; 32]) -> [u8; 32] {
            lhs.iter_mut().zip(rhs).for_each(|(lhs, rhs)| *lhs ^= rhs);
            lhs
        }

        randomness.into_iter().fold([0; 32], xor)
    }

    fn save_peg_out_signatures(
        &self,
        mut batch: BatchTx,
        nonces: Vec<(PeerId, PegOutNonceItem)>,
        signatures: Vec<(PeerId, PegOutSignatureItem)>,
    ) {
        let mut cache: BTreeMap<Txid, UnsignedTransaction> = self
            .db
            .find_by_prefix(&UnsignedTransactionPrefixKey)
            .map(|res| {
                let (key, val) = res.expect("DB error");
                (key.0, val)
            })
            .collect();

        for (peer, nonce) in nonces.into_iter() {
            match cache.get_mut(&nonce.txid) {
                Some(unsigned) => unsigned.nonces.push((peer, nonce)),
                None => warn!(
                    "{} sent peg-out nonce for unknown PSBT {}",
                    peer, nonce.txid
                ),
            }
        }

        for (peer, sig) in signatures.into_iter() {
            match cache.get_mut(&sig.txid) {
                Some(unsigned) => unsigned.signatures.push((peer, sig)),
                None => warn!(
                    "{} sent peg-out signature for unknown PSBT {}",
                    peer, sig.txid
                ),
            }
        }

        for (txid, unsigned) in cache.into_iter() {
            batch.append_insert(UnsignedTransactionKey(txid), unsigned);
        }
        batch.commit();
    }

    fn _finalize_peg_out_psbt(
        &self,
        psbt: &mut PartiallySignedTransaction,
        change: Amount,
    ) -> Result<PendingTransaction, ProcessPegOutSigError> {
        // We need to save the change output's tweak key to be able to access the funds later on.
        // The tweak is extracted here because the psbt is moved next and not available anymore
        // when the tweak is actually needed in the end to be put into the batch on success.
        let change_tweak: [u8; 32] = psbt
            .outputs
            .iter()
            .flat_map(|output| output.proprietary.get(&proprietary_tweak_key()).cloned())
            .next()
            .ok_or(ProcessPegOutSigError::MissingOrMalformedChangeTweak)?
            .try_into()
            .map_err(|_| ProcessPegOutSigError::MissingOrMalformedChangeTweak)?;

        if let Err(error) = psbt.finalize_mut(&self.secp) {
            return Err(ProcessPegOutSigError::ErrorFinalizingPsbt(error));
        }

        let tx = psbt.clone().extract_tx();

        Ok(PendingTransaction {
            tx,
            tweak: change_tweak,
            change,
        })
    }

    /// # Panics
    /// * If proposals is empty
    async fn process_fee_proposals(&self, mut proposals: Vec<Feerate>) -> Feerate {
        assert!(!proposals.is_empty());

        proposals.sort();

        *proposals
            .get(proposals.len() / 2)
            .expect("We checked before that proposals aren't empty")
    }

    /// # Panics
    /// * If proposals is empty
    async fn process_block_height_proposals(
        &self,
        batch: BatchTx<'_>,
        mut proposals: Vec<u32>,
    ) -> u32 {
        assert!(!proposals.is_empty());

        proposals.sort_unstable();
        let median_proposal = proposals[proposals.len() / 2];

        let consensus_height = self.consensus_height().unwrap_or(0);

        if median_proposal >= consensus_height {
            debug!("Setting consensus block height to {}", median_proposal);
            self.sync_up_to_consensus_height(batch, median_proposal)
                .await;
        } else {
            panic!(
                "Median proposed consensus block height shrunk from {} to {}, the federation is broken",
                consensus_height, median_proposal
            );
        }

        median_proposal
    }

    pub fn current_round_consensus(&self) -> Option<RoundConsensus> {
        self.db.get_value(&RoundConsensusKey).expect("DB error")
    }

    pub async fn target_height(&self) -> u32 {
        let our_network_height = self.btc_rpc.get_block_height().await as u32;
        our_network_height.saturating_sub(self.cfg.finality_delay)
    }

    pub fn consensus_height(&self) -> Option<u32> {
        self.current_round_consensus().map(|rc| rc.block_height)
    }

    async fn sync_up_to_consensus_height(&self, mut batch: BatchTx<'_>, new_height: u32) {
        let old_height = self.consensus_height().unwrap_or(0);
        if new_height < old_height {
            info!(
                new_height,
                old_height, "Nothing to sync, new height is lower than old height, doing nothing."
            );
            return;
        }

        if new_height == old_height {
            debug!(height = old_height, "Height didn't change");
            return;
        }

        info!(
            new_height,
            block_to_go = new_height - old_height,
            "New consensus height, syncing up",
        );

        batch.reserve((new_height - old_height) as usize + 1);
        for height in (old_height + 1)..=(new_height) {
            if height % 100 == 0 {
                debug!("Caught up to block {}", height);
            }

            // TODO: use batching for mainnet syncing
            trace!(block = height, "Fetching block hash");
            let block_hash = self.btc_rpc.get_block_hash(height as u64).await; // TODO: use u64 for height everywhere

            let pending_transactions = self
                .db
                .find_by_prefix(&PendingTransactionPrefixKey)
                .map(|res| {
                    let (key, transaction) = res.expect("DB error");
                    (key.0, transaction)
                })
                .collect::<HashMap<_, _>>();

            if !pending_transactions.is_empty() {
                let block = self.btc_rpc.get_block(&block_hash).await;
                for transaction in block.txdata {
                    if let Some(pending_tx) = pending_transactions.get(&transaction.txid()) {
                        self.recognize_change_utxo(batch.subtransaction(), pending_tx);
                    }
                }
            }

            batch.append_insert_new(
                BlockHashKey(BlockHash::from_inner(block_hash.into_inner())),
                (),
            );
        }
        batch.commit();
    }

    /// Add a change UTXO to our spendable UTXO database after it was included in a block that we
    /// got consensus on.
    fn recognize_change_utxo(&self, mut batch: BatchTx, pending_tx: &PendingTransaction) {
        let script_pk = self
            .cfg
            .peg_in_descriptor
            .tweak(&pending_tx.tweak, &self.secp)
            .script_pubkey();
        for (idx, output) in pending_tx.tx.output.iter().enumerate() {
            if output.script_pubkey == script_pk {
                batch.append_insert(
                    UTXOKey(bitcoin::OutPoint {
                        txid: pending_tx.tx.txid(),
                        vout: idx as u32,
                    }),
                    SpendableUTXO {
                        tweak: pending_tx.tweak,
                        amount: bitcoin::Amount::from_sat(output.value),
                    },
                )
            }
        }
        batch.commit();
    }

    fn block_is_known(&self, block_hash: BlockHash) -> bool {
        self.db
            .get_value(&BlockHashKey(block_hash))
            .expect("DB error")
            .is_some()
    }

    fn create_peg_out_tx(&self, peg_out: &PegOut) -> Option<UnsignedTransaction> {
        let change_tweak = self.current_round_consensus().unwrap().randomness_beacon;
        self.offline_wallet().create_tx(
            peg_out.amount,
            peg_out.recipient.script_pubkey(),
            self.available_utxos(),
            peg_out.fees.fee_rate,
            &change_tweak,
        )
    }

    fn available_utxos(&self) -> Vec<(UTXOKey, SpendableUTXO)> {
        self.db
            .find_by_prefix(&UTXOPrefixKey)
            .collect::<Result<_, _>>()
            .expect("DB error")
    }

    pub fn get_wallet_value(&self) -> bitcoin::Amount {
        let sat_sum = self
            .available_utxos()
            .into_iter()
            .map(|(_, utxo)| utxo.amount.as_sat())
            .sum();
        bitcoin::Amount::from_sat(sat_sum)
    }

    fn offline_wallet(&self) -> StatelessWallet {
        StatelessWallet {
            descriptor: &self.cfg.peg_in_descriptor,
            secp: &self.secp,
        }
    }

    fn create_sign_session(
        &self,
        tx: &UnsignedTransaction,
        input_index: usize,
    ) -> (frost::SignSession, frost::XOnlyFrostKey, [u8; 32]) {
        let frost_instance = frost::new_frost();
        let frost_key = self.cfg.frost_key.clone();

        let tweak_pk_bytes = tx.psbt.inputs[input_index]
            .proprietary
            .get(&proprietary_tweak_key())
            .expect("Malformed PSBT: expected tweak");

        let tweak = {
            let mut hasher = HmacEngine::<sha256::Hash>::new(&frost_key.public_key().to_bytes());
            hasher.input(&tweak_pk_bytes[..]);
            Hmac::from_engine(hasher).into_inner()
        };

        let frost_key = frost_key
            .tweak(frost::Scalar::from_bytes_mod_order(tweak))
            .expect("computationally unreachable");

        let mut tx_hasher = SighashCache::new(&tx.psbt.unsigned_tx);
        let prevouts = tx
            .psbt
            .inputs
            .iter()
            .map(|input| input.witness_utxo.as_ref().expect("must exist"))
            .collect::<Vec<_>>();
        let prevouts = bitcoin::util::sighash::Prevouts::All(&prevouts);
        let message = tx_hasher
            .taproot_key_spend_signature_hash(input_index, &prevouts, SchnorrSighashType::Default)
            .expect("sighash should be infallible");

        let frost_key = frost_key.into_xonly_key();

        let tr_tweak =
            taproot::TapTweakHash::from_key_and_tweak(frost_key.public_key().into(), None);
        let tr_tweaked_key = frost_key
            .tweak(frost::Scalar::from_bytes_mod_order(tr_tweak.into_inner()))
            .expect("computationally unreachable");
        let peer_nonces_for_input = tx
            .nonces
            .iter()
            .map(|(peer_id, nonces)| (peer_id.to_usize() as u32, nonces.nonces[input_index].0))
            .collect();

        (
            frost_instance.start_sign_session(
                &tr_tweaked_key,
                peer_nonces_for_input,
                frost::Message::raw(&message[..]),
            ),
            tr_tweaked_key,
            message.into_inner(),
        )
    }
}

impl<'a> StatelessWallet<'a> {
    /// Attempts to create a tx ready to be signed from available UTXOs.
    /// Returns `None` if there are not enough `SpendableUTXO`
    fn create_tx(
        &self,
        peg_out_amount: bitcoin::Amount,
        destination: Script,
        mut utxos: Vec<(UTXOKey, SpendableUTXO)>,
        fee_rate: Feerate,
        change_tweak: &[u8],
    ) -> Option<UnsignedTransaction> {
        // When building a transaction we need to take care of two things:
        //  * We need enough input amount to fund all outputs
        //  * We need to keep an eye on the tx weight so we can factor the fees into out calculation
        // We then go on to calculate the base size of the transaction `total_weight` and the
        // maximum weight per added input which we will add every time we select an input.
        let change_script = self.derive_script(change_tweak);
        let out_weight = (destination.len() * 4 + 1 + 32
            // Add change script weight, it's very likely to be needed if not we just overpay in fees
            + 1 // script len varint, 1 byte for all addresses we accept
            + change_script.len() * 4 // script len
            + 32) as u64; // value
        let mut total_weight = (16 + // version
            12 + // up to 2**16-1 inputs
            12 + // up to 2**16-1 outputs
            out_weight + // weight of all outputs
            16) as u64; // lock time
        let max_input_weight = (self
            .descriptor
            .max_satisfaction_weight()
            .expect("is satisfyable") +
            128 + // TxOutHash
            16 + // TxOutIndex
            16) as u64; // sequence

        // Finally we initialize our accumulator for selected input amounts
        let mut total_selected_value = bitcoin::Amount::from_sat(0);
        let mut selected_utxos: Vec<(UTXOKey, SpendableUTXO)> = vec![];
        let mut fees = fee_rate.calculate_fee(total_weight);

        // When selecting UTXOs we select from largest to smallest amounts
        utxos.sort_by_key(|(_, utxo)| utxo.amount);
        while total_selected_value < peg_out_amount + change_script.dust_value() + fees {
            match utxos.pop() {
                Some((utxo_key, utxo)) => {
                    total_selected_value += utxo.amount;
                    total_weight += max_input_weight;
                    fees = fee_rate.calculate_fee(total_weight);
                    selected_utxos.push((utxo_key, utxo));
                }
                _ => return None, // Not enough UTXOs
            }
        }

        // We always pay ourselves change back to ensure that we don't lose anything due to dust
        let change = total_selected_value - fees - peg_out_amount;
        let output: Vec<TxOut> = vec![
            TxOut {
                value: peg_out_amount.as_sat(),
                script_pubkey: destination,
            },
            TxOut {
                value: change.as_sat(),
                script_pubkey: change_script,
            },
        ];
        let mut change_out = bitcoin::util::psbt::Output::default();
        change_out
            .proprietary
            .insert(proprietary_tweak_key(), change_tweak.to_vec());

        info!(
            inputs = selected_utxos.len(),
            input_sats = total_selected_value.as_sat(),
            peg_out_sats = peg_out_amount.as_sat(),
            fees_sats = fees.as_sat(),
            fee_rate = fee_rate.sats_per_kvb,
            change_sats = change.as_sat(),
            "Creating peg-out tx",
        );

        let transaction = Transaction {
            version: 2,
            lock_time: 0,
            input: selected_utxos
                .iter()
                .map(|(utxo_key, _utxo)| TxIn {
                    previous_output: utxo_key.0,
                    script_sig: Default::default(),
                    sequence: 0xFFFFFFFF,
                    witness: bitcoin::Witness::new(),
                })
                .collect(),
            output,
        };
        info!(txid = %transaction.txid(), "Creating peg-out tx");

        // FIXME: use custom data structure that guarantees more invariants and only convert to PSBT for finalization
        let psbt = PartiallySignedTransaction {
            unsigned_tx: transaction,
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs: selected_utxos
                .into_iter()
                .map(|(_utxo_key, utxo)| {
                    let script_pubkey = self
                        .descriptor
                        .tweak(&utxo.tweak, self.secp)
                        .script_pubkey();
                    Input {
                        non_witness_utxo: None,
                        witness_utxo: Some(TxOut {
                            value: utxo.amount.as_sat(),
                            script_pubkey,
                        }),
                        partial_sigs: Default::default(),
                        sighash_type: None,
                        redeem_script: None,
                        witness_script: None,
                        bip32_derivation: Default::default(),
                        final_script_sig: None,
                        final_script_witness: None,
                        ripemd160_preimages: Default::default(),
                        sha256_preimages: Default::default(),
                        hash160_preimages: Default::default(),
                        hash256_preimages: Default::default(),
                        proprietary: vec![(proprietary_tweak_key(), utxo.tweak.to_vec())]
                            .into_iter()
                            .collect(),
                        tap_key_sig: Default::default(),
                        tap_script_sigs: Default::default(),
                        tap_scripts: Default::default(),
                        tap_key_origins: Default::default(),
                        tap_internal_key: Default::default(),
                        tap_merkle_root: Default::default(),
                        unknown: Default::default(),
                    }
                })
                .collect(),
            outputs: vec![Default::default(), change_out],
        };

        Some(UnsignedTransaction {
            psbt,
            signatures: vec![],
            nonces: vec![],
            change,
            fees: PegOutFees {
                fee_rate,
                total_weight,
            },
        })
    }

    // fn sign_psbt(&self, psbt: &mut PartiallySignedTransaction) {
    //     let mut tx_hasher = SighashCache::new(&psbt.unsigned_tx);
    // }

    fn derive_script(&self, tweak: &[u8]) -> Script {
        let descriptor = self.descriptor.translate_pk3_infallible(|pub_key| {
            let hashed_tweak = {
                let mut hasher = HmacEngine::<sha256::Hash>::new(&pub_key.serialize()[..]);
                hasher.input(tweak);
                Hmac::from_engine(hasher).into_inner()
            };

            let mut tweak_key = pub_key.clone();
            tweak_key
                .add_exp_assign(self.secp, &hashed_tweak)
                .expect("tweaking failed");
            tweak_key
        });

        descriptor.script_pubkey()
    }
}

fn proprietary_tweak_key() -> ProprietaryKey {
    ProprietaryKey {
        prefix: b"minimint".to_vec(),
        subtype: 0x00,
        key: vec![],
    }
}

pub fn is_address_valid_for_network(address: &Address, network: Network) -> bool {
    match (address.network, address.address_type()) {
        (Network::Testnet, Some(AddressType::P2pkh))
        | (Network::Testnet, Some(AddressType::P2sh)) => {
            [Network::Testnet, Network::Regtest, Network::Signet].contains(&network)
        }
        (Network::Testnet, _) => [Network::Testnet, Network::Signet].contains(&network),
        (addr_net, _) => addr_net == network,
    }
}

#[instrument(level = "debug", skip_all)]
pub async fn run_broadcast_pending_tx(db: Arc<dyn Database>, rpc: Box<dyn BitcoindRpc>) {
    loop {
        broadcast_pending_tx(&db, rpc.as_ref()).await;
        minimint_api::task::sleep(Duration::from_secs(10)).await;
    }
}

pub async fn broadcast_pending_tx(db: &Arc<dyn Database>, rpc: &dyn BitcoindRpc) {
    let pending_tx = db
        .find_by_prefix(&PendingTransactionPrefixKey)
        .collect::<Result<Vec<_>, _>>()
        .expect("DB error");

    for (_, PendingTransaction { tx, .. }) in pending_tx {
        debug!(
            tx = %tx.txid(),
            weight = tx.weight(),
            "Broadcasting peg-out",
        );
        trace!(transaction = ?tx);
        rpc.submit_transaction(tx).await;
    }
}

impl Feerate {
    pub fn calculate_fee(&self, weight: u64) -> bitcoin::Amount {
        let sats = self.sats_per_kvb * weight / 1000;
        bitcoin::Amount::from_sat(sats)
    }
}

impl std::hash::Hash for PegOutSignatureItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.txid.hash(state);
        // for sig in self.signature.iter() {
        //     sig.serialize_der().hash(state);
        // }
    }
}

impl PartialEq for PegOutSignatureItem {
    fn eq(&self, other: &PegOutSignatureItem) -> bool {
        self.txid == other.txid && self.signatures == other.signatures
    }
}

impl Eq for PegOutSignatureItem {}

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("Connected bitcoind is on wrong network, expected {0}, got {1}")]
    WrongNetwork(Network, Network),
    #[error("Error querying bitcoind: {0}")]
    RpcError(#[from] anyhow::Error),
    #[error("Unknown bitcoin network: {0}")]
    UnknownNetwork(String),
    #[error("Unknown block hash in peg-in proof: {0}")]
    UnknownPegInProofBlock(BlockHash),
    #[error("Invalid peg-in proof: {0}")]
    PegInProofError(#[from] PegInProofError),
    #[error("The peg-in was already claimed")]
    PegInAlreadyClaimed,
    #[error("Peg-out fee rate {0:?} is set below consensus {1:?}")]
    PegOutFeeRate(Feerate, Feerate),
    #[error("Not enough SpendableUTXO")]
    NotEnoughSpendableUTXO,
}

#[derive(Debug, Error)]
pub enum ProcessPegOutSigError {
    #[error("No unsigned transaction with id {0} exists")]
    UnknownTransaction(Txid),
    #[error("Expected {0} signatures, got {1}")]
    WrongSignatureCount(usize, usize),
    #[error("Bad Sighash")]
    SighashError,
    #[error("Malformed signature: {0}")]
    MalformedSignature(secp256k1::Error),
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Duplicate signature")]
    DuplicateSignature,
    #[error("Missing change tweak")]
    MissingOrMalformedChangeTweak,
    #[error("Error finalizing PSBT {0:?}")]
    ErrorFinalizingPsbt(Vec<miniscript::psbt::Error>),
}

// FIXME: make FakeFed not require Eq
/// **WARNING**: this is only intended to be used for testing
impl PartialEq for WalletError {
    fn eq(&self, other: &Self) -> bool {
        format!("{:?}", self) == format!("{:?}", other)
    }
}

/// **WARNING**: this is only intended to be used for testing
impl Eq for WalletError {}
