// Copyright © Aptos Foundation

use crate::{
    establish_connection_pool,
    models::{
        nft_metadata_crawler_uris::NFTMetadataCrawlerURIs,
        nft_metadata_crawler_uris_query::NFTMetadataCrawlerURIsQuery,
    },
    utils::{
        database::upsert_uris,
        gcs::{write_image_to_gcs, write_json_to_gcs},
        image_optimizer::ImageOptimizer,
        json_parser::JSONParser,
        uri_parser::URIParser,
    },
};
use aptos_indexer_grpc_server_framework::RunnableConfig;
use chrono::NaiveDateTime;
use crossbeam_channel::{bounded, Receiver, Sender};
use diesel::{
    r2d2::{ConnectionManager, Pool, PooledConnection},
    PgConnection,
};
use futures::StreamExt;
use google_cloud_pubsub::{
    client::{Client, ClientConfig},
    subscription::Subscription,
};
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinHandle,
    time::sleep,
};
use tracing::{error, info};

/// Structs to hold config from YAML
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParserConfig {
    pub google_application_credentials: String,
    pub bucket: String,
    pub subscription_name: String,
    pub database_url: String,
    pub cdn_prefix: String,
    pub ipfs_prefix: String,
    pub num_parsers: usize,
    pub max_file_size_bytes: u32,
    pub image_quality: u8, // Quality up to 100
}

/// Subscribes to PubSub and sends URIs to Channel
/// - Creates an infinite loop that pulls `msgs_per_pull` entries from PubSub
/// - Parses each entry into a `Worker` and sends to Channel
async fn consume_pubsub_entries_to_channel_loop(
    parser_config: ParserConfig,
    sender: Sender<(Worker, String)>,
    subscription: Subscription,
    pool: Pool<ConnectionManager<PgConnection>>,
) -> anyhow::Result<()> {
    let mut stream = subscription.subscribe(None).await?;
    while let Some(msg) = stream.next().await {
        // Parse metadata from Pubsub message and create worker
        let ack = msg.ack_id();
        let entry_string = String::from_utf8(msg.message.clone().data)?;
        let parts: Vec<&str> = entry_string.split(',').collect();
        let entry = Worker::new(
            parser_config.clone(),
            pool.get()?,
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string().parse()?,
            NaiveDateTime::parse_from_str(parts[3], "%Y-%m-%d %H:%M:%S %Z").unwrap_or(
                NaiveDateTime::parse_from_str(parts[3], "%Y-%m-%d %H:%M:%S%.f %Z")?,
            ),
            parts[4].parse::<bool>().unwrap_or(false),
            parser_config.max_file_size_bytes,
            parser_config.image_quality,
        );

        // Send worker to channel
        sender.send((entry, ack.to_string())).unwrap_or_else(|e| {
            error!("Failed to send entry to channel: {:?}", e);
        });
    }

    Ok(())
}

/// Repeatedly pulls workers from Channel and perform parsing operations
async fn spawn_parser(
    semaphore: Arc<Semaphore>,
    receiver: Arc<Mutex<Receiver<(Worker, String)>>>,
    subscription: Subscription,
) -> anyhow::Result<()> {
    loop {
        let _ = semaphore.acquire().await?;

        // Pulls worker from Channel
        let (mut parser, ack) = receiver.lock().await.recv()?;
        parser.parse().await?;

        // Sends ack to PubSub
        subscription.ack(vec![ack]).await?;

        sleep(Duration::from_millis(500)).await;
    }
}

#[async_trait::async_trait]
impl RunnableConfig for ParserConfig {
    /// Main driver function that establishes a connection to Pubsub and parses the Pubsub entries in parallel
    async fn run(&self) -> anyhow::Result<()> {
        let pool = establish_connection_pool(self.database_url.clone());

        std::env::set_var(
            "GOOGLE_APPLICATION_CREDENTIALS",
            self.google_application_credentials.clone(),
        );

        // Establish gRPC client
        let config = ClientConfig::default().with_auth().await?;
        let client = Client::new(config).await?;
        let subscription = client.subscription(&self.subscription_name);

        // Create workers
        let (sender, receiver) = bounded::<(Worker, String)>(2 * self.num_parsers);
        let receiver = Arc::new(Mutex::new(receiver));
        let semaphore = Arc::new(Semaphore::new(self.num_parsers));

        // Spawn producer
        let producer = tokio::spawn(consume_pubsub_entries_to_channel_loop(
            self.clone(),
            sender,
            subscription.clone(),
            pool,
        ));

        // Spawns workers
        let mut workers: Vec<JoinHandle<anyhow::Result<()>>> = Vec::new();
        for _ in 0..self.num_parsers {
            let worker = tokio::spawn(spawn_parser(
                Arc::clone(&semaphore),
                Arc::clone(&receiver),
                subscription.clone(),
            ));

            workers.push(worker);
        }

        match producer.await {
            Ok(_) => (),
            Err(e) => error!("[NFT Metadata Crawler] Producer error: {:?}", e),
        }

        for worker in workers {
            match worker.await {
                Ok(_) => (),
                Err(e) => error!("[NFT Metadata Crawler] Worker error: {:?}", e),
            }
        }
        Ok(())
    }

    fn get_server_name(&self) -> String {
        "parser".to_string()
    }
}

/// Stuct that represents a parser for a single entry from queue
#[allow(dead_code)] // Will remove when functions are implemented
pub struct Worker {
    config: ParserConfig,
    conn: PooledConnection<ConnectionManager<PgConnection>>,
    model: NFTMetadataCrawlerURIs,
    token_data_id: String,
    token_uri: String,
    last_transaction_version: i32,
    last_transaction_timestamp: chrono::NaiveDateTime,
    force: bool,
    max_file_size_bytes: u32,
    image_quality: u8,
}

impl Worker {
    pub fn new(
        config: ParserConfig,
        conn: PooledConnection<ConnectionManager<PgConnection>>,
        token_data_id: String,
        token_uri: String,
        last_transaction_version: i32,
        last_transaction_timestamp: chrono::NaiveDateTime,
        force: bool,
        max_file_size_bytes: u32,
        image_quality: u8,
    ) -> Self {
        Self {
            config,
            conn,
            model: NFTMetadataCrawlerURIs::new(token_uri.clone()),
            token_data_id,
            token_uri,
            last_transaction_version,
            last_transaction_timestamp,
            force,
            max_file_size_bytes,
            image_quality,
        }
    }

    /// Main parsing flow
    pub async fn parse(&mut self) -> anyhow::Result<()> {
        info!(
            last_transaction_version = self.last_transaction_version,
            "[NFT Metadata Crawler] Starting parser"
        );

        // Deduplicate token_uri
        // Proceed if force or if token_uri has not been parsed
        if self.force
            || NFTMetadataCrawlerURIsQuery::get_by_token_uri(
                self.token_uri.clone(),
                &mut self.conn,
            )?
            .is_none()
        {
            // Parse token_uri
            self.model.set_token_uri(self.token_uri.clone());
            let token_uri = self.model.get_token_uri();
            let json_uri = URIParser::parse(token_uri.clone()).unwrap_or(token_uri);

            // Parse JSON for raw_image_uri and raw_animation_uri
            let (raw_image_uri, raw_animation_uri, json) =
                JSONParser::parse(json_uri, self.max_file_size_bytes)
                    .await
                    .unwrap_or_else(|e| {
                        // Increment retry count if JSON parsing fails
                        error!(
                            last_transaction_version = self.last_transaction_version,
                            error = ?e,
                            "[NFT Metadata Crawler] JSON parse failed",
                        );
                        self.model.increment_json_parser_retry_count();
                        (None, None, Value::Null)
                    });

            self.model.set_raw_image_uri(raw_image_uri);
            self.model.set_raw_animation_uri(raw_animation_uri);

            // Save parsed JSON to GCS
            if json != Value::Null {
                let cdn_json_uri =
                    write_json_to_gcs(self.config.bucket.clone(), self.token_data_id.clone(), json)
                        .await
                        .ok();
                self.model.set_cdn_json_uri(cdn_json_uri);
            }

            // Commit model to Postgres
            if let Err(e) = upsert_uris(&mut self.conn, self.model.clone()) {
                error!(
                    last_transaction_version = self.last_transaction_version,
                    error = ?e,
                    "[NFT Metadata Crawler] Commit to Postgres failed"
                );
            }
        }

        // Deduplicate raw_image_uri
        // Proceed with image optimization of force or if raw_image_uri has not been parsed
        if self.force
            || self.model.get_raw_image_uri().map_or(true, |uri_option| {
                NFTMetadataCrawlerURIsQuery::get_by_raw_image_uri(uri_option, &mut self.conn)
                    .map_or(true, |uri| uri.is_none())
            })
        {
            // Parse raw_image_uri, use token_uri if parsing fails
            let raw_image_uri = self
                .model
                .get_raw_image_uri()
                .unwrap_or(self.model.get_token_uri());
            let img_uri = URIParser::parse(raw_image_uri).unwrap_or(self.model.get_token_uri());

            // Resize and optimize image and animation
            let (image, format) =
                ImageOptimizer::optimize(img_uri, self.max_file_size_bytes, self.image_quality)
                    .await
                    .unwrap_or_else(|e| {
                        // Increment retry count if image is None
                        error!(
                            last_transaction_version = self.last_transaction_version,
                            error = ?e,
                            "[NFT Metadata Crawler] Image optimization failed"
                        );
                        self.model.increment_image_optimizer_retry_count();
                        (vec![], ImageFormat::Png)
                    });

            if !image.is_empty() {
                // Save resized and optimized image to GCS
                let cdn_image_uri = write_image_to_gcs(
                    format,
                    self.config.bucket.clone(),
                    self.token_data_id.clone(),
                    image,
                )
                .await
                .ok();
                self.model.set_cdn_image_uri(cdn_image_uri);
            }

            // Commit model to Postgres
            if let Err(e) = upsert_uris(&mut self.conn, self.model.clone()) {
                error!(
                    last_transaction_version = self.last_transaction_version,
                    error = ?e,
                    "[NFT Metadata Crawler] Commit to Postgres failed"
                );
            }
        }

        // Deduplicate raw_animation_uri
        // Set raw_animation_uri_option to None if not force and raw_animation_uri already exists
        let mut raw_animation_uri_option = self.model.get_raw_animation_uri();
        if !self.force
            && raw_animation_uri_option.clone().map_or(true, |uri| {
                NFTMetadataCrawlerURIsQuery::get_by_raw_animation_uri(uri, &mut self.conn)
                    .unwrap_or(None)
                    .is_some()
            })
        {
            raw_animation_uri_option = None;
        }

        // If raw_animation_uri_option is None, skip
        if let Some(raw_animation_uri) = raw_animation_uri_option {
            let animation_uri =
                URIParser::parse(raw_animation_uri.clone()).unwrap_or(raw_animation_uri);

            // Resize and optimize animation
            let (animation, format) = ImageOptimizer::optimize(
                animation_uri,
                self.max_file_size_bytes,
                self.image_quality,
            )
            .await
            .unwrap_or_else(|e| {
                // Increment retry count if animation is None
                error!(
                    last_transaction_version = self.last_transaction_version,
                    error = ?e,
                    "[NFT Metadata Crawler] Animation optimization failed"
                );
                self.model.increment_animation_optimizer_retry_count();
                (vec![], ImageFormat::Png)
            });

            // Save resized and optimized animation to GCS
            if !animation.is_empty() {
                let cdn_animation_uri = write_image_to_gcs(
                    format,
                    self.config.bucket.clone(),
                    self.token_data_id.clone(),
                    animation,
                )
                .await
                .ok();
                self.model.set_cdn_animation_uri(cdn_animation_uri);
            }

            // Commit model to Postgres
            if let Err(e) = upsert_uris(&mut self.conn, self.model.clone()) {
                error!(
                    last_transaction_version = self.last_transaction_version,
                    error = ?e,
                    "[NFT Metadata Crawler] Commit to Postgres failed"
                );
            }
        }

        Ok(())
    }
}