use anyhow::Result;
use async_trait::async_trait;
use std::{cell::RefCell, sync::{Arc, RwLock}, time::Duration};
use reqwest::{Client, StatusCode, Response};
use rusty_s3::{Bucket, Credentials, S3Action};
use tokio::sync::Mutex;
use y_sweet_core::store::Store;


const PRESIGNED_URL_DURATION_SECONDS: u64 = 60 * 60;
pub struct S3Store {
    bucket: Bucket,
    _bucket_inited: Arc<Mutex<bool>>,
    client: Client,
    credentials: Credentials,
    prefix: Option<String>,
    presigned_url_duration: Duration,
}

impl S3Store {
    pub fn new(
        region: String,
        bucket_name: String,
        prefix: Option<String>,
        aws_access_key_id: String,
        aws_secret: String,
    ) -> Self {
        let credentials = Credentials::new(aws_access_key_id, aws_secret);
        let endpoint = format!("https://s3.dualstack.{}.amazonaws.com", region)
            .parse()
            .expect("endpoint is a valid Url");
        let path_style = rusty_s3::UrlStyle::VirtualHost;
        let bucket = Bucket::new(endpoint, path_style, bucket_name, region)
            .expect("Url has a valid scheme and host");
        let client = Client::new();

        let presigned_url_duration = Duration::from_secs(PRESIGNED_URL_DURATION_SECONDS);
        S3Store {
            bucket,
            _bucket_inited: Arc::new(Mutex::new(false)),
            client,
            credentials,
            prefix,
            presigned_url_duration,
        }
    }

	async fn head_bucket(&self) -> Result<Response> {
		let action = self.bucket.head_bucket(Some(&self.credentials));
		let presigned_url =
			action.sign(self.presigned_url_duration);
		let response = self.client.head(presigned_url).send().await?;
		Ok(response)
	}

	//lazily checks bucket exists on first use
    async fn inited_bucket(&self) -> Result<&Bucket> {
		let mut bucket_inited = self._bucket_inited.lock().await;
        if !*bucket_inited {
			let response = self.head_bucket().await?;
            match response.status() {
                StatusCode::OK => {
					*bucket_inited = true;
                }
                StatusCode::NOT_FOUND => {
                    return Err(anyhow::anyhow!(
                        "No such bucket {} exists!",
                        self.bucket.name()
                    ))
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Other AWS Error: Code {} Err {}",
						response.status(),
						response.text().await?
                    ))
                }
            }
        }
        Ok(&self.bucket)
    }

    fn prefixed_key(&self, key: &str) -> String {
        if let Some(path_prefix) = &self.prefix {
            format!("{}/{}", path_prefix, key)
        } else {
            key.to_string()
        }
    }
}

#[async_trait]
impl Store for S3Store {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let bucket = self.inited_bucket().await?;
        let prefixed_key = self.prefixed_key(key);
        let object_get = bucket.get_object(Some(&self.credentials), &prefixed_key);
        let presigned_url =
            object_get.sign(self.presigned_url_duration);
        let response = self.client.get(presigned_url).send().await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => Ok(Some(response.bytes().await?.to_vec())),
            _ => Err(anyhow::anyhow!(
                "Other AWS Error: Code {} Err {}",
                response.status(),
                response.text().await?
            )),
        }
    }

    async fn set(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let _code = self.bucket.put_object(&self.make_key(key), &value).await?;
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let _code = self.bucket.delete_object(&self.make_key(key)).await?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let response = self.bucket.head_object(&self.make_key(key)).await;

        match response {
            Ok(_) => Ok(true),
            Err(S3Error::Http(404, _)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
