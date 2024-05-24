use std::{num::NonZeroU64, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use hypersync_net_types::{ArchiveHeight, Query};
use polars_arrow::{array::Array, record_batch::RecordBatch as Chunk};
use reqwest::Method;

mod column_mapping;
mod config;
mod decode;
mod from_arrow;
mod parquet_out;
mod parse_response;
pub mod preset_query;
mod rayon_async;
pub mod simple_types;
mod stream;
mod types;
mod util;

pub use from_arrow::FromArrow;
pub use hypersync_format as format;
pub use hypersync_net_types as net_types;
pub use hypersync_schema as schema;

use parse_response::parse_query_response;
use simple_types::Event;
use tokio::sync::mpsc;
use types::{EventResponse, ResponseData};
use url::Url;

pub use column_mapping::{ColumnMapping, DataType};
pub use config::{ClientConfig, StreamConfig};
pub use decode::Decoder;
pub use types::{ArrowBatch, ArrowResponse, ArrowResponseData, QueryResponse};

type ArrowChunk = Chunk<Box<dyn Array>>;

#[derive(Clone)]
pub struct Client {
    http_client: reqwest::Client,
    url: Url,
    bearer_token: Option<String>,
    max_num_retries: usize,
    retry_backoff_ms: u64,
    retry_base_ms: u64,
    retry_ceiling_ms: u64,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        let timeout = cfg
            .http_req_timeout_millis
            .unwrap_or(NonZeroU64::new(30_000).unwrap());

        let http_client = reqwest::Client::builder()
            .no_gzip()
            .timeout(Duration::from_millis(timeout.get()))
            .build()
            .unwrap();

        Ok(Self {
            http_client,
            url: cfg
                .url
                .unwrap_or("https://eth.hypersync.xyz".parse().context("parse url")?),
            bearer_token: cfg.bearer_token,
            max_num_retries: cfg.max_num_retries.unwrap_or(12),
            retry_backoff_ms: cfg.retry_backoff_ms.unwrap_or(500),
            retry_base_ms: cfg.retry_base_ms.unwrap_or(200),
            retry_ceiling_ms: cfg.retry_ceiling_ms.unwrap_or(5_000),
        })
    }

    pub async fn collect(
        self: Arc<Self>,
        query: Query,
        config: StreamConfig,
    ) -> Result<QueryResponse> {
        check_simple_stream_params(&config)?;

        let mut recv = stream::stream_arrow(self, query, config)
            .await
            .context("start stream")?;

        let mut data = ResponseData::default();
        let mut archive_height = None;
        let mut next_block = 0;
        let mut total_execution_time = 0;

        while let Some(res) = recv.recv().await {
            let res = res.context("get response")?;
            let res: QueryResponse = QueryResponse::from(&res);

            for batch in res.data.blocks {
                data.blocks.push(batch);
            }
            for batch in res.data.transactions {
                data.transactions.push(batch);
            }
            for batch in res.data.logs {
                data.logs.push(batch);
            }
            for batch in res.data.traces {
                data.traces.push(batch);
            }

            archive_height = res.archive_height;
            next_block = res.next_block;
            total_execution_time += res.total_execution_time
        }

        Ok(QueryResponse {
            archive_height,
            next_block,
            total_execution_time,
            data,
            rollback_guard: None,
        })
    }

    pub async fn collect_events(
        self: Arc<Self>,
        mut query: Query,
        config: StreamConfig,
    ) -> Result<EventResponse> {
        check_simple_stream_params(&config)?;

        add_event_join_fields_to_selection(&mut query);

        let mut recv = stream::stream_arrow(self, query, config)
            .await
            .context("start stream")?;

        let mut data = Vec::new();
        let mut archive_height = None;
        let mut next_block = 0;
        let mut total_execution_time = 0;

        while let Some(res) = recv.recv().await {
            let res = res.context("get response")?;
            let res: QueryResponse = QueryResponse::from(&res);
            let events: Vec<Event> = res.data.into();

            data.push(events);

            archive_height = res.archive_height;
            next_block = res.next_block;
            total_execution_time += res.total_execution_time
        }

        Ok(EventResponse {
            archive_height,
            next_block,
            total_execution_time,
            data,
            rollback_guard: None,
        })
    }

    pub async fn collect_arrow(
        self: Arc<Self>,
        query: Query,
        config: StreamConfig,
    ) -> Result<ArrowResponse> {
        let mut recv = stream::stream_arrow(self, query, config)
            .await
            .context("start stream")?;

        let mut data = ArrowResponseData::default();
        let mut archive_height = None;
        let mut next_block = 0;
        let mut total_execution_time = 0;

        while let Some(res) = recv.recv().await {
            let res = res.context("get response")?;

            for batch in res.data.blocks {
                data.blocks.push(batch);
            }
            for batch in res.data.transactions {
                data.transactions.push(batch);
            }
            for batch in res.data.logs {
                data.logs.push(batch);
            }
            for batch in res.data.traces {
                data.traces.push(batch);
            }
            for batch in res.data.decoded_logs {
                data.decoded_logs.push(batch);
            }

            archive_height = res.archive_height;
            next_block = res.next_block;
            total_execution_time += res.total_execution_time
        }

        Ok(ArrowResponse {
            archive_height,
            next_block,
            total_execution_time,
            data,
            rollback_guard: None,
        })
    }

    pub async fn collect_parquet(
        self: Arc<Self>,
        path: &str,
        query: Query,
        config: StreamConfig,
    ) -> Result<()> {
        parquet_out::collect_parquet(self, path, query, config).await
    }

    async fn get_height_impl(&self) -> Result<u64> {
        let mut url = self.url.clone();
        let mut segments = url.path_segments_mut().ok().context("get path segments")?;
        segments.push("height");
        std::mem::drop(segments);
        let mut req = self.http_client.request(Method::GET, url);

        if let Some(bearer_token) = &self.bearer_token {
            req = req.bearer_auth(bearer_token);
        }

        let res = req.send().await.context("execute http req")?;

        let status = res.status();
        if !status.is_success() {
            return Err(anyhow!("http response status code {}", status));
        }

        let height: ArchiveHeight = res.json().await.context("read response body json")?;

        Ok(height.height.unwrap_or(0))
    }

    pub async fn get_height(&self) -> Result<u64> {
        let mut base = self.retry_base_ms;

        let mut err = anyhow!("");

        for _ in 0..self.max_num_retries {
            match self.get_height_impl().await {
                Ok(res) => return Ok(res),
                Err(e) => {
                    log::error!(
                        "failed to get height from server, retrying... The error was: {:?}",
                        e
                    );
                    err = err.context(e);
                }
            }

            let base_ms = Duration::from_millis(base);
            let jitter = Duration::from_millis(fastrange_rs::fastrange_64(
                rand::random(),
                self.retry_backoff_ms,
            ));

            tokio::time::sleep(base_ms + jitter).await;

            base = std::cmp::min(base + self.retry_backoff_ms, self.retry_ceiling_ms);
        }

        Err(err)
    }

    pub async fn get(&self, query: &Query) -> Result<QueryResponse> {
        let arrow_response = self.get_arrow(query).await.context("get data")?;
        Ok(QueryResponse::from(&arrow_response))
    }

    pub async fn get_events(&self, mut query: Query) -> Result<EventResponse> {
        add_event_join_fields_to_selection(&mut query);
        let arrow_response = self.get_arrow(&query).await.context("get data")?;
        Ok(EventResponse::from(&arrow_response))
    }

    async fn get_arrow_impl(&self, query: &Query) -> Result<ArrowResponse> {
        let mut url = self.url.clone();
        let mut segments = url.path_segments_mut().ok().context("get path segments")?;
        segments.push("query");
        segments.push("arrow-ipc");
        std::mem::drop(segments);
        let mut req = self.http_client.request(Method::POST, url);

        if let Some(bearer_token) = &self.bearer_token {
            req = req.bearer_auth(bearer_token);
        }

        let res = req.json(&query).send().await.context("execute http req")?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.context("read text to see error")?;

            return Err(anyhow!(
                "http response status code {}, err body: {}",
                status,
                text
            ));
        }

        let bytes = res.bytes().await.context("read response body bytes")?;

        let res = tokio::task::block_in_place(|| {
            parse_query_response(&bytes).context("parse query response")
        })?;

        Ok(res)
    }

    pub async fn get_arrow(&self, query: &Query) -> Result<ArrowResponse> {
        let mut base = self.retry_base_ms;

        let mut err = anyhow!("");

        for _ in 0..self.max_num_retries {
            match self.get_arrow_impl(query).await {
                Ok(res) => return Ok(res),
                Err(e) => {
                    log::error!(
                        "failed to get height from server, retrying... The error was: {:?}",
                        e
                    );
                    err = err.context(e);
                }
            }

            let base_ms = Duration::from_millis(base);
            let jitter = Duration::from_millis(fastrange_rs::fastrange_64(
                rand::random(),
                self.retry_backoff_ms,
            ));

            tokio::time::sleep(base_ms + jitter).await;

            base = std::cmp::min(base + self.retry_backoff_ms, self.retry_ceiling_ms);
        }

        Err(err)
    }

    pub async fn stream(
        self: Arc<Self>,
        query: Query,
        config: StreamConfig,
    ) -> Result<mpsc::Receiver<Result<QueryResponse>>> {
        check_simple_stream_params(&config)?;

        let (tx, rx): (_, mpsc::Receiver<Result<QueryResponse>>) =
            mpsc::channel(config.concurrency.unwrap_or(10));

        let mut inner_rx = self
            .stream_arrow(query, config)
            .await
            .context("start inner stream")?;

        tokio::spawn(async move {
            while let Some(resp) = inner_rx.recv().await {
                let is_err = resp.is_err();
                if tx
                    .send(resp.map(|r| QueryResponse::from(&r)))
                    .await
                    .is_err()
                    || is_err
                {
                    return;
                }
            }
        });

        Ok(rx)
    }

    pub async fn stream_events(
        self: Arc<Self>,
        mut query: Query,
        config: StreamConfig,
    ) -> Result<mpsc::Receiver<Result<EventResponse>>> {
        check_simple_stream_params(&config)?;

        add_event_join_fields_to_selection(&mut query);

        let (tx, rx): (_, mpsc::Receiver<Result<EventResponse>>) =
            mpsc::channel(config.concurrency.unwrap_or(10));

        let mut inner_rx = self
            .stream_arrow(query, config)
            .await
            .context("start inner stream")?;

        tokio::spawn(async move {
            while let Some(resp) = inner_rx.recv().await {
                let is_err = resp.is_err();
                if tx
                    .send(resp.map(|r| EventResponse::from(&r)))
                    .await
                    .is_err()
                    || is_err
                {
                    return;
                }
            }
        });

        Ok(rx)
    }

    pub async fn stream_arrow(
        self: Arc<Self>,
        query: Query,
        config: StreamConfig,
    ) -> Result<mpsc::Receiver<Result<ArrowResponse>>> {
        stream::stream_arrow(self, query, config).await
    }
}

fn check_simple_stream_params(config: &StreamConfig) -> Result<()> {
    if config.event_signature.is_some() {
        return Err(anyhow!("config.event_signature can't be passed to simple type function. User is expected to decode the logs using Decoder."));
    }
    if config.column_mapping.is_some() {
        return Err(anyhow!("config.column_mapping can't be passed to single type function. User is expected to map values manually."));
    }

    Ok(())
}

fn add_event_join_fields_to_selection(query: &mut Query) {
    // Field lists for implementing event based API, these fields are used for joining
    // so they should always be added to the field selection.
    const BLOCK_JOIN_FIELDS: &[&str] = &["number"];
    const TX_JOIN_FIELDS: &[&str] = &["block_number", "transaction_index"];
    const LOG_JOIN_FIELDS: &[&str] = &["log_index", "transaction_index", "block_number"];

    if !query.field_selection.block.is_empty() {
        for field in BLOCK_JOIN_FIELDS.iter() {
            query.field_selection.block.insert(field.to_string());
        }
    }

    if !query.field_selection.transaction.is_empty() {
        for field in TX_JOIN_FIELDS.iter() {
            query.field_selection.transaction.insert(field.to_string());
        }
    }

    if !query.field_selection.log.is_empty() {
        for field in LOG_JOIN_FIELDS.iter() {
            query.field_selection.log.insert(field.to_string());
        }
    }
}
