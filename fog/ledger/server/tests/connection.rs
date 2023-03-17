// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Integration tests at the level of the fog ledger connection / fog ledger
//! grpc API

use mc_account_keys::{AccountKey, PublicAddress};
use mc_api::watcher::TimestampResultCode;
use mc_attest_net::{Client as AttestClient, RaClient};
use mc_attest_verifier::{MrSignerVerifier, Verifier, DEBUG_ENCLAVE};
use mc_blockchain_types::{BlockSignature, BlockVersion};
use mc_common::{
    logger::{test_with_logger, Logger},
    time::SystemTimeProvider,
    ResponderId,
};
use mc_crypto_keys::{CompressedRistrettoPublic, Ed25519Pair};
use mc_fog_api::ledger::TxOutResultCode;
use mc_fog_ledger_connection::{
    Error, FogKeyImageGrpcClient, FogMerkleProofGrpcClient, FogUntrustedLedgerGrpcClient,
    KeyImageResultExtension, OutputResultExtension,
};
use mc_fog_ledger_enclave::LedgerSgxEnclave;
use mc_fog_ledger_server::{LedgerServer, LedgerServerConfig};
use mc_fog_test_infra::get_enclave_path;
use mc_fog_uri::{ConnectionUri, FogLedgerUri};
use mc_ledger_db::{test_utils::recreate_ledger_db, Ledger, LedgerDB};
use mc_transaction_core::{
    membership_proofs::compute_implied_merkle_root, ring_signature::KeyImage, tokens::Mob, Amount,
    Token,
};
use mc_util_from_random::FromRandom;
use mc_util_grpc::{GrpcRetryConfig, CHAIN_ID_MISMATCH_ERR_MSG};
use mc_util_test_helper::{CryptoRng, RngCore, RngType, SeedableRng};
use mc_watcher::watcher_db::WatcherDB;
use std::{path::PathBuf, str::FromStr, sync::Arc, thread::sleep, time::Duration};
use tempfile::TempDir;
use url::Url;

const TEST_URL: &str = "http://www.my_url1.com";

const OMAP_CAPACITY: u64 = 128 * 128;

const GRPC_RETRY_CONFIG: GrpcRetryConfig = GrpcRetryConfig {
    grpc_retry_count: 3,
    grpc_retry_millis: 20,
};

fn setup_watcher_db(logger: Logger) -> (WatcherDB, PathBuf) {
    let url = Url::parse(TEST_URL).unwrap();

    let db_tmp = TempDir::new().expect("Could not make tempdir for wallet db");
    WatcherDB::create(db_tmp.path()).unwrap();
    let watcher = WatcherDB::open_rw(db_tmp.path(), &[url], logger).unwrap();
    let watcher_dir = db_tmp.path().to_path_buf();
    (watcher, watcher_dir)
}

// Test that a fog ledger connection is able to get valid merkle proofs by
// hitting a fog ledger server
#[test_with_logger]
fn fog_ledger_merkle_proofs_test(logger: Logger) {
    let base_port = 3230;

    let mut rng = RngType::from_seed([0u8; 32]);

    for block_version in BlockVersion::iterator() {
        let alice = AccountKey::random_with_fog(&mut rng);
        let bob = AccountKey::random_with_fog(&mut rng);
        let charlie = AccountKey::random_with_fog(&mut rng);

        let recipients = vec![
            alice.default_subaddress(),
            bob.default_subaddress(),
            charlie.default_subaddress(),
        ];

        // Make LedgerDB
        let ledger_dir = TempDir::new().expect("Could not get test_ledger tempdir");
        let db_full_path = ledger_dir.path();
        let mut ledger = recreate_ledger_db(db_full_path);

        let (mut watcher, watcher_dir) = setup_watcher_db(logger.clone());

        // Populate ledger with some data
        add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &[],
            &mut rng,
            &mut watcher,
        );
        add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &[KeyImage::from(1)],
            &mut rng,
            &mut watcher,
        );
        let num_blocks = add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &[KeyImage::from(2)],
            &mut rng,
            &mut watcher,
        );

        {
            // Make LedgerServer
            let client_uri = FogLedgerUri::from_str(&format!(
                "insecure-fog-ledger://127.0.0.1:{}",
                base_port + 7
            ))
            .unwrap();
            let config = LedgerServerConfig {
                chain_id: "local".to_string(),
                ledger_db: db_full_path.to_path_buf(),
                watcher_db: watcher_dir,
                admin_listen_uri: Default::default(),
                client_listen_uri: client_uri.clone(),
                client_responder_id: ResponderId::from_str(&client_uri.addr()).unwrap(),
                ias_spid: Default::default(),
                ias_api_key: Default::default(),
                client_auth_token_secret: None,
                client_auth_token_max_lifetime: Default::default(),
                omap_capacity: OMAP_CAPACITY,
            };

            let enclave = LedgerSgxEnclave::new(
                get_enclave_path(mc_fog_ledger_enclave::ENCLAVE_FILE),
                &config.client_responder_id,
                OMAP_CAPACITY,
                logger.clone(),
            );

            let ra_client =
                AttestClient::new(&config.ias_api_key).expect("Could not create IAS client");

            let grpc_env = Arc::new(grpcio::EnvBuilder::new().build());

            let mut ledger_server = LedgerServer::new(
                config,
                enclave,
                ledger.clone(),
                watcher.clone(),
                ra_client,
                SystemTimeProvider::default(),
                logger.clone(),
            );

            ledger_server
                .start()
                .expect("Failed starting ledger server");

            // Make ledger enclave client
            let mut mr_signer_verifier =
                MrSignerVerifier::from(mc_fog_ledger_enclave_measurement::sigstruct());
            mr_signer_verifier.allow_hardening_advisories(
                mc_fog_ledger_enclave_measurement::HARDENING_ADVISORIES,
            );

            let mut verifier = Verifier::default();
            verifier.mr_signer(mr_signer_verifier).debug(DEBUG_ENCLAVE);

            let mut client = FogMerkleProofGrpcClient::new(
                "local".to_string(),
                client_uri.clone(),
                GRPC_RETRY_CONFIG,
                verifier.clone(),
                grpc_env.clone(),
                logger.clone(),
            );

            // Get merkle root of num_blocks - 1
            let merkle_root = {
                let temp = ledger.get_tx_out_proof_of_memberships(&[0u64]).unwrap();
                let merkle_proof = &temp[0];
                compute_implied_merkle_root(merkle_proof).unwrap()
            };

            // Get some tx outs and merkle proofs
            let response = client
                .get_outputs(
                    vec![0u64, 1u64, 2u64, 3u64, 4u64, 5u64, 6u64, 7u64, 8u64],
                    num_blocks - 1,
                )
                .expect("get outputs failed");

            // Test the basic fields
            assert_eq!(response.num_blocks, num_blocks);
            assert_eq!(response.global_txo_count, ledger.num_txos().unwrap());

            // Validate merkle proofs
            for res in response.results.iter() {
                let (tx_out, proof) = res.status().unwrap().unwrap();
                let result = mc_transaction_core::membership_proofs::is_membership_proof_valid(
                    &tx_out,
                    &proof,
                    merkle_root.hash.as_ref(),
                )
                .expect("membership proof structure failed!");
                assert!(result, "membership proof was invalid! idx = {}, output = {:?}, proof = {:?}, merkle_root = {:?}", res.index, tx_out, proof, merkle_root);
            }

            // Make some queries that are out of bounds
            let response = client
                .get_outputs(vec![1u64, 6u64, 9u64, 14u64], num_blocks - 1)
                .expect("get outputs failed");

            // Test the basic fields
            assert_eq!(response.num_blocks, num_blocks);
            assert_eq!(response.global_txo_count, ledger.num_txos().unwrap());
            assert_eq!(response.results.len(), 4);
            assert!(response.results[0].status().as_ref().unwrap().is_some());
            assert!(response.results[1].status().as_ref().unwrap().is_some());
            assert!(response.results[2].status().as_ref().unwrap().is_none());
            assert!(response.results[3].status().as_ref().unwrap().is_none());

            // Check that wrong chain id results in an error
            let mut client = FogMerkleProofGrpcClient::new(
                "wrong".to_string(),
                client_uri,
                GRPC_RETRY_CONFIG,
                verifier,
                grpc_env,
                logger.clone(),
            );

            let result = client.get_outputs(
                vec![0u64, 1u64, 2u64, 3u64, 4u64, 5u64, 6u64, 7u64, 8u64],
                num_blocks - 1,
            );

            if let Err(err) = result {
                match err {
                    Error::Connection(
                        _,
                        retry::Error {
                            error:
                                mc_fog_enclave_connection::Error::Rpc(grpcio::Error::RpcFailure(status)),
                            ..
                        },
                    ) => {
                        let expected = format!("{} '{}'", CHAIN_ID_MISMATCH_ERR_MSG, "local");
                        assert_eq!(status.message(), expected);
                    }
                    _ => {
                        panic!("unexpected grpcio error: {err}");
                    }
                }
            } else {
                panic!("Expected an error when chain-id is wrong");
            }
        }
        // grpcio detaches all its threads and does not join them :(
        // we opened a PR here: https://github.com/tikv/grpc-rs/pull/455
        // in the meantime we can just sleep after grpcio env and all related
        // objects have been destroyed, and hope that those 6 threads see the
        // shutdown requests within 1 second.
        sleep(Duration::from_millis(1000));
    }
}

// Test that a fog ledger connection is able to check key images by hitting
// a fog ledger server
#[test_with_logger]
fn fog_ledger_key_images_test(logger: Logger) {
    let base_port = 3240;

    let mut rng = RngType::from_seed([0u8; 32]);

    for block_version in BlockVersion::iterator() {
        let alice = AccountKey::random_with_fog(&mut rng);

        let recipients = vec![alice.default_subaddress()];

        let keys: Vec<KeyImage> = (0..20).map(|x| KeyImage::from(x as u64)).collect();

        // Make LedgerDB
        let ledger_dir = TempDir::new().expect("Could not get test_ledger tempdir");
        let db_full_path = ledger_dir.path();
        let mut ledger = recreate_ledger_db(db_full_path);

        // Make WatcherDB
        let (mut watcher, watcher_dir) = setup_watcher_db(logger.clone());

        // Populate ledger with some data
        // Origin block cannot have key images
        add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &[],
            &mut rng,
            &mut watcher,
        );
        add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &keys[0..2],
            &mut rng,
            &mut watcher,
        );
        add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &keys[3..6],
            &mut rng,
            &mut watcher,
        );
        let num_blocks = add_block_to_ledger(
            block_version,
            &mut ledger,
            &recipients,
            &keys[6..9],
            &mut rng,
            &mut watcher,
        );

        // Populate watcher with Signature and Timestamp for block 1
        let url1 = Url::parse(TEST_URL).unwrap();
        let block1 = ledger.get_block(1).unwrap();
        let signing_key_a = Ed25519Pair::from_random(&mut rng);
        let filename = String::from("00/00");
        let mut signed_block_a1 =
            BlockSignature::from_block_and_keypair(&block1, &signing_key_a).unwrap();
        signed_block_a1.set_signed_at(1593798844);
        watcher
            .add_block_signature(&url1, 1, signed_block_a1, filename.clone())
            .unwrap();

        // Update last synced to block 2, to indicate that this URL did not participate
        // in consensus for block 2.
        watcher.update_last_synced(&url1, 2).unwrap();

        {
            // Make LedgerServer
            let client_uri = FogLedgerUri::from_str(&format!(
                "insecure-fog-ledger://127.0.0.1:{}",
                base_port + 7
            ))
            .unwrap();
            let config = LedgerServerConfig {
                chain_id: "local".to_string(),
                ledger_db: db_full_path.to_path_buf(),
                watcher_db: watcher_dir,
                admin_listen_uri: Default::default(),
                client_listen_uri: client_uri.clone(),
                client_responder_id: ResponderId::from_str(&client_uri.addr()).unwrap(),
                ias_spid: Default::default(),
                ias_api_key: Default::default(),
                client_auth_token_secret: None,
                client_auth_token_max_lifetime: Default::default(),
                omap_capacity: OMAP_CAPACITY,
            };

            let enclave = LedgerSgxEnclave::new(
                get_enclave_path(mc_fog_ledger_enclave::ENCLAVE_FILE),
                &config.client_responder_id,
                OMAP_CAPACITY,
                logger.clone(),
            );

            let ra_client =
                AttestClient::new(&config.ias_api_key).expect("Could not create IAS client");

            let grpc_env = Arc::new(grpcio::EnvBuilder::new().build());

            let mut ledger_server = LedgerServer::new(
                config,
                enclave,
                ledger.clone(),
                watcher,
                ra_client,
                SystemTimeProvider::default(),
                logger.clone(),
            );

            ledger_server
                .start()
                .expect("Failed starting ledger server");

            // Make ledger enclave client
            let mut mr_signer_verifier =
                MrSignerVerifier::from(mc_fog_ledger_enclave_measurement::sigstruct());
            mr_signer_verifier.allow_hardening_advisories(
                mc_fog_ledger_enclave_measurement::HARDENING_ADVISORIES,
            );

            let mut verifier = Verifier::default();
            verifier.mr_signer(mr_signer_verifier).debug(DEBUG_ENCLAVE);

            let mut client = FogKeyImageGrpcClient::new(
                String::default(),
                client_uri,
                GRPC_RETRY_CONFIG,
                verifier,
                grpc_env,
                logger.clone(),
            );

            // Check on key images
            let mut response = client
                .check_key_images(&[keys[0], keys[1], keys[3], keys[7], keys[19]])
                .expect("check_key_images failed");

            let mut n = 1;
            // adding a delay to give fog ledger time to fully initialize
            while response.num_blocks != num_blocks {
                response = client
                    .check_key_images(&[keys[0], keys[1], keys[3], keys[7], keys[19]])
                    .expect("check_key_images failed");

                sleep(Duration::from_secs(10));
                // panic on the 20th time
                n += 1; //
                if n > 20 {
                    panic!("Fog ledger not  fully initialized");
                }
            }

            // FIXME assert_eq!(response.num_txos, ...);
            assert_eq!(response.results[0].key_image, keys[0]);
            assert_eq!(response.results[0].status(), Ok(Some(1)));
            assert_eq!(
                response.results[0].timestamp_result_code,
                TimestampResultCode::TimestampFound as u32
            );
            assert_eq!(response.results[0].timestamp, 1);

            assert_eq!(response.results[1].key_image, keys[1]);
            assert_eq!(response.results[1].status(), Ok(Some(1)));
            assert_eq!(
                response.results[1].timestamp_result_code,
                TimestampResultCode::TimestampFound as u32
            );
            assert_eq!(response.results[1].timestamp, 1);

            // Check a key_image for a block which will never have signatures & timestamps
            assert_eq!(response.results[2].key_image, keys[3]);
            assert_eq!(response.results[2].status(), Ok(Some(2))); // Spent in block 2
            assert_eq!(
                response.results[2].timestamp_result_code,
                TimestampResultCode::TimestampFound as u32
            );
            assert_eq!(response.results[2].timestamp, 2);

            // Watcher has only synced 1 block, so timestamp should be behind
            assert_eq!(response.results[3].key_image, keys[7]);
            assert_eq!(response.results[3].status(), Ok(Some(3))); // Spent in block 3
            assert_eq!(
                response.results[3].timestamp_result_code,
                TimestampResultCode::TimestampFound as u32
            );
            assert_eq!(response.results[3].timestamp, 3);

            // Check a key_image that has not been spent
            assert_eq!(response.results[4].key_image, keys[19]);
            assert_eq!(response.results[4].status(), Ok(None)); // Not spent
            assert_eq!(
                response.results[4].timestamp_result_code,
                TimestampResultCode::TimestampFound as u32
            );
            assert_eq!(response.results[4].timestamp, u64::MAX);
        }

        // FIXME: Check a key_image that generates a DatabaseError - tough to generate

        // grpcio detaches all its threads and does not join them :(
        // we opened a PR here: https://github.com/tikv/grpc-rs/pull/455
        // in the meantime we can just sleep after grpcio env and all related
        // objects have been destroyed, and hope that those 6 threads see the
        // shutdown requests within 1 second.
        sleep(Duration::from_millis(1000));
    }
}

// Test that a fog ledger connection is able to check key images by hitting
// a fog ledger server
#[test_with_logger]
fn fog_ledger_blocks_api_test(logger: Logger) {
    let base_port = 3250;

    let mut rng = RngType::from_seed([0u8; 32]);

    let alice = AccountKey::random_with_fog(&mut rng);
    let bob = AccountKey::random_with_fog(&mut rng);
    let charlie = AccountKey::random_with_fog(&mut rng);

    let recipients = vec![alice.default_subaddress()];

    // Make LedgerDB
    let ledger_dir = TempDir::new().expect("Could not get test_ledger tempdir");
    let db_full_path = ledger_dir.path();
    let mut ledger = recreate_ledger_db(db_full_path);

    let (mut watcher, watcher_dir) = setup_watcher_db(logger.clone());

    // Populate ledger with some data
    // Origin block cannot have key images
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[alice.default_subaddress()],
        &[],
        &mut rng,
        &mut watcher,
    );
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[alice.default_subaddress(), bob.default_subaddress()],
        &[KeyImage::from(1)],
        &mut rng,
        &mut watcher,
    );
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[
            alice.default_subaddress(),
            bob.default_subaddress(),
            charlie.default_subaddress(),
        ],
        &[KeyImage::from(2)],
        &mut rng,
        &mut watcher,
    );
    let num_blocks = add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &recipients,
        &[KeyImage::from(3)],
        &mut rng,
        &mut watcher,
    );

    {
        // Make LedgerServer
        let client_uri = FogLedgerUri::from_str(&format!(
            "insecure-fog-ledger://127.0.0.1:{}",
            base_port + 7
        ))
        .unwrap();
        let config = LedgerServerConfig {
            chain_id: "local".to_string(),
            ledger_db: db_full_path.to_path_buf(),
            watcher_db: watcher_dir,
            admin_listen_uri: Default::default(),
            client_listen_uri: client_uri.clone(),
            client_responder_id: ResponderId::from_str(&client_uri.addr()).unwrap(),
            ias_spid: Default::default(),
            ias_api_key: Default::default(),
            client_auth_token_secret: None,
            client_auth_token_max_lifetime: Default::default(),
            omap_capacity: OMAP_CAPACITY,
        };

        let enclave = LedgerSgxEnclave::new(
            get_enclave_path(mc_fog_ledger_enclave::ENCLAVE_FILE),
            &config.client_responder_id,
            OMAP_CAPACITY,
            logger.clone(),
        );

        let ra_client =
            AttestClient::new(&config.ias_api_key).expect("Could not create IAS client");

        let grpc_env = Arc::new(grpcio::EnvBuilder::new().build());

        let mut ledger_server = LedgerServer::new(
            config,
            enclave,
            ledger.clone(),
            watcher,
            ra_client,
            SystemTimeProvider::default(),
            logger.clone(),
        );

        ledger_server
            .start()
            .expect("Failed starting ledger server");

        // Make unattested ledger client
        let client =
            FogUntrustedLedgerGrpcClient::new(client_uri, GRPC_RETRY_CONFIG, grpc_env, logger);

        // Try to get a block
        let queries = [0..1];
        let result = client.get_blocks(&queries).unwrap();
        // Check that we got 1 block, as expected
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.blocks[0].index, 0);
        assert_eq!(result.blocks[0].outputs.len(), 1);
        assert_eq!(result.blocks[0].global_txo_count, 1);
        assert_eq!(
            result.blocks[0].timestamp_result_code,
            TimestampResultCode::BlockIndexOutOfBounds as u32
        );
        assert_eq!(result.num_blocks, num_blocks);
        assert_eq!(result.global_txo_count, ledger.num_txos().unwrap());

        // Try to get two blocks
        let queries = [1..3];
        let result = client.get_blocks(&queries).unwrap();

        // Check that we got 2 blocks, as expected
        assert_eq!(result.blocks.len(), 2);
        assert_eq!(result.blocks[0].index, 1);
        assert_eq!(result.blocks[0].outputs.len(), 2);
        assert_eq!(result.blocks[0].global_txo_count, 3);
        assert_eq!(
            result.blocks[0].timestamp_result_code,
            TimestampResultCode::TimestampFound as u32
        );
        assert_eq!(result.blocks[1].index, 2);
        assert_eq!(result.blocks[1].outputs.len(), 3);
        assert_eq!(result.blocks[1].global_txo_count, 6);
        assert_eq!(
            result.blocks[1].timestamp_result_code,
            TimestampResultCode::TimestampFound as u32
        );
        assert_eq!(result.num_blocks, num_blocks);
        assert_eq!(result.global_txo_count, ledger.num_txos().unwrap());
    }

    // grpcio detaches all its threads and does not join them :(
    // we opened a PR here: https://github.com/tikv/grpc-rs/pull/455
    // in the meantime we can just sleep after grpcio env and all related
    // objects have been destroyed, and hope that those 6 threads see the
    // shutdown requests within 1 second.
    sleep(Duration::from_millis(1000));
}

// Test that a fog ledger connection is able to check key images by hitting
// a fog ledger server
#[test_with_logger]
fn fog_ledger_untrusted_tx_out_api_test(logger: Logger) {
    let base_port = 3260;

    let mut rng = RngType::from_seed([0u8; 32]);

    let alice = AccountKey::random_with_fog(&mut rng);
    let bob = AccountKey::random_with_fog(&mut rng);
    let charlie = AccountKey::random_with_fog(&mut rng);

    let recipients = vec![alice.default_subaddress()];

    // Make LedgerDB
    let ledger_dir = TempDir::new().expect("Could not get test_ledger tempdir");
    let db_full_path = ledger_dir.path();
    let mut ledger = recreate_ledger_db(db_full_path);

    let (mut watcher, watcher_dir) = setup_watcher_db(logger.clone());

    // Populate ledger with some data
    // Origin block cannot have key images
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[alice.default_subaddress()],
        &[],
        &mut rng,
        &mut watcher,
    );
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[alice.default_subaddress(), bob.default_subaddress()],
        &[KeyImage::from(1)],
        &mut rng,
        &mut watcher,
    );
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &[
            alice.default_subaddress(),
            bob.default_subaddress(),
            charlie.default_subaddress(),
        ],
        &[KeyImage::from(2)],
        &mut rng,
        &mut watcher,
    );
    add_block_to_ledger(
        BlockVersion::MAX,
        &mut ledger,
        &recipients,
        &[KeyImage::from(3)],
        &mut rng,
        &mut watcher,
    );

    {
        // Make LedgerServer
        let client_uri = FogLedgerUri::from_str(&format!(
            "insecure-fog-ledger://127.0.0.1:{}",
            base_port + 7
        ))
        .unwrap();
        let config = LedgerServerConfig {
            chain_id: "local".to_string(),
            ledger_db: db_full_path.to_path_buf(),
            watcher_db: watcher_dir,
            admin_listen_uri: Default::default(),
            client_listen_uri: client_uri.clone(),
            client_responder_id: ResponderId::from_str(&client_uri.addr()).unwrap(),
            ias_spid: Default::default(),
            ias_api_key: Default::default(),
            client_auth_token_secret: None,
            client_auth_token_max_lifetime: Default::default(),
            omap_capacity: OMAP_CAPACITY,
        };

        let enclave = LedgerSgxEnclave::new(
            get_enclave_path(mc_fog_ledger_enclave::ENCLAVE_FILE),
            &config.client_responder_id,
            OMAP_CAPACITY,
            logger.clone(),
        );

        let ra_client =
            AttestClient::new(&config.ias_api_key).expect("Could not create IAS client");

        let grpc_env = Arc::new(grpcio::EnvBuilder::new().build());

        let mut ledger_server = LedgerServer::new(
            config,
            enclave,
            ledger.clone(),
            watcher,
            ra_client,
            SystemTimeProvider::default(),
            logger.clone(),
        );

        ledger_server
            .start()
            .expect("Failed starting ledger server");

        // Make unattested ledger client
        let client =
            FogUntrustedLedgerGrpcClient::new(client_uri, GRPC_RETRY_CONFIG, grpc_env, logger);

        // Get a tx_out that is actually in the ledger
        let real_tx_out0 = { ledger.get_tx_out_by_index(0).unwrap() };

        // Try to get tx out records
        let queries: Vec<CompressedRistrettoPublic> =
            vec![(&[0u8; 32]).into(), real_tx_out0.public_key];
        let result = client.get_tx_outs(queries).unwrap();
        // Check that we got expected num_blocks value
        assert_eq!(result.num_blocks, 4);
        // Check that we got 2 results, as expected
        assert_eq!(result.results.len(), 2);
        assert_eq!(
            &result.results[0].tx_out_pubkey.clone().unwrap().data[..],
            &[0u8; 32]
        );
        assert_eq!(result.results[0].result_code, TxOutResultCode::NotFound);
        assert_eq!(
            &result.results[1].tx_out_pubkey.clone().unwrap().data[..],
            &real_tx_out0.public_key.as_bytes()[..]
        );
        assert_eq!(result.results[1].result_code, TxOutResultCode::Found);
        assert_eq!(result.results[1].tx_out_global_index, 0);
        assert_eq!(result.results[1].block_index, 0);
        assert_eq!(
            result.results[1].timestamp_result_code,
            TimestampResultCode::BlockIndexOutOfBounds as u32
        );
    }

    // grpcio detaches all its threads and does not join them :(
    // we opened a PR here: https://github.com/tikv/grpc-rs/pull/455
    // in the meantime we can just sleep after grpcio env and all related
    // objects have been destroyed, and hope that those 6 threads see the
    // shutdown requests within 1 second.
    sleep(Duration::from_millis(1000));
}

// Infra

/// Adds a block containing one txo for each provided recipient and returns new
/// block height.
///
/// # Arguments
/// * `ledger_db`
/// * `recipients` - Recipients of outputs.
/// * `rng`
fn add_block_to_ledger(
    block_version: BlockVersion,
    ledger_db: &mut LedgerDB,
    recipients: &[PublicAddress],
    key_images: &[KeyImage],
    rng: &mut (impl CryptoRng + RngCore),
    watcher: &mut WatcherDB,
) -> u64 {
    let amount = Amount::new(10, Mob::ID);
    let block_data = mc_ledger_db::test_utils::add_block_to_ledger(
        ledger_db,
        block_version,
        recipients,
        amount,
        key_images,
        rng,
    )
    .expect("failed to add block");
    let block_index = block_data.block().index;

    let signature = block_data.signature().expect("missing signature");
    for src_url in watcher.get_config_urls().unwrap().iter() {
        watcher
            .add_block_signature(
                src_url,
                block_index,
                signature.clone(),
                format!("00/{block_index}"),
            )
            .expect("Could not add block signature");
    }

    block_index + 1
}
