use std::{
    env::temp_dir,
    path::{Path, PathBuf, absolute},
};

use aws_config::{BehaviorVersion, SdkConfig, meta::region::RegionProviderChain};
use aws_sdk_s3::primitives::ByteStream;
use lambda_http::{Body, Error, Request, Response};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

use crate::encrypted::{FileCondition, get_file_condition};

const DEFAULT_X2T_PATH: &str = "/var/www/onlyoffice/documentserver/server/FileConverter/bin";
const DEFAULT_FONTS_PATH: &str = "/var/www/onlyoffice/documentserver/fonts";

#[cfg(not(windows))]
const X2T_BIN: &str = "x2t";
#[cfg(windows)]
const X2T_BIN: &str = "x2t.exe";

/// This is the main body for the function.
/// Write your code inside it.
/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
pub(crate) async fn function_handler(event: Request) -> Result<Response<Body>, Error> {
    let aws_config = aws_config().await;
    let s3_client = aws_sdk_s3::Client::new(&aws_config);

    let body = event.body();
    let request: ConvertRequest = serde_json::from_slice(body)?;

    let mut x2t_path: Option<PathBuf> = None;
    let mut fonts_path: Option<PathBuf> = None;

    // Try loading paths from environment variables
    if x2t_path.is_none()
        && let Ok(path) = std::env::var("X2T_PATH")
    {
        x2t_path = Some(PathBuf::from(&path));
    }

    if fonts_path.is_none()
        && let Ok(path) = std::env::var("X2T_FONTS_PATH")
    {
        fonts_path = Some(PathBuf::from(&path));
    }

    // Try determine default path
    if x2t_path.is_none() {
        let default_path = Path::new(DEFAULT_X2T_PATH);

        if default_path.is_dir() {
            x2t_path = Some(default_path.to_path_buf());
        }
    }

    if fonts_path.is_none() {
        let default_path = Path::new(DEFAULT_FONTS_PATH);
        fonts_path = Some(default_path.to_path_buf());
    }

    // Check a path was provided
    let x2t_path = match x2t_path {
        Some(value) => absolute(value)?,
        None => {
            tracing::error!("no x2t install path provided, cannot start server");
            panic!();
        }
    };

    let fonts_path = match fonts_path {
        Some(value) => absolute(value)?,
        None => {
            tracing::error!("no fonts path provided, cannot start server");
            panic!();
        }
    };

    let temp_path = temp_dir().join("onlyoffice-convert-server");

    // Ensure temporary path exists
    if !temp_path.exists() {
        tokio::fs::create_dir_all(&temp_path).await.map_err(|err| {
            tracing::error!(?err, "failed to create temporary directory");
            std::io::Error::other("failed to create temporary directory")
        })?;
    }

    // Create temporary path
    let paths = create_convert_temp_paths(&temp_path).map_err(|err| {
        tracing::error!(?err, "failed to setup temporary paths");
        std::io::Error::other("failed to setup temporary file paths")
    })?;

    // Generate the convert config
    let config = format!(
        r#"
        <?xml version="1.0" encoding="utf-8"?>
        <TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                              xmlns:xsd="http://www.w3.org/2001/XMLSchema">
          <m_sFileFrom>{}</m_sFileFrom>
          <m_sFileTo>{}</m_sFileTo>
          <m_sFontDir>{}</m_sFontDir>
          <m_nFormatTo>513</m_nFormatTo>
        </TaskQueueDataConvert>
        "#,
        paths.input_path.display(),
        paths.output_path.display(),
        fonts_path.display(),
    );

    let result = x2t(X2tInput {
        s3_client: &s3_client,
        paths: &paths,
        request,
        config_bytes: config.as_bytes(),
        x2t_path: &x2t_path,
    })
    .await;

    // Spawn a cleanup task
    tokio::spawn(async move {
        if paths.input_path.exists()
            && let Err(err) = tokio::fs::remove_file(paths.input_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }

        if paths.config_path.exists()
            && let Err(err) = tokio::fs::remove_file(paths.config_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }

        if paths.output_path.exists()
            && let Err(err) = tokio::fs::remove_file(paths.output_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }
    });

    if let Err(error) = result {
        let body = serde_json::to_string(&error)?;
        // Return something that implements IntoResponse.
        // It will be serialized to the right response event automatically by the runtime
        let resp = Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(body.into())
            .map_err(Box::new)?;
        return Ok(resp);
    }

    // Return something that implements IntoResponse.
    // It will be serialized to the right response event automatically by the runtime
    let resp = Response::builder()
        .status(200)
        .body(().into())
        .map_err(Box::new)?;
    Ok(resp)
}

struct X2tInput<'a> {
    s3_client: &'a aws_sdk_s3::Client,
    paths: &'a ConvertTempPaths,
    request: ConvertRequest,
    config_bytes: &'a [u8],
    x2t_path: &'a Path,
}

async fn x2t(input: X2tInput<'_>) -> Result<(), ErrorResponse> {
    tracing::debug!("writing config file");

    // Write the config file to disk
    tokio::fs::write(&input.paths.config_path, input.config_bytes)
        .await
        .map_err(|err| {
            tracing::error!(?err, "failed to write config file");
            ErrorResponse {
                reason: Some("WRITE_CONFIG_FILE"),
                x2t_code: None,
                message: "failed to write config file".to_string(),
            }
        })?;

    tracing::debug!("streaming source file");

    // Stream the input file to disk
    stream_source_file(
        input.s3_client,
        input.request.source_bucket,
        input.request.source_key,
        &input.paths.input_path,
    )
    .await?;

    let x2t = input.x2t_path.join(X2T_BIN);
    let x2t = x2t.to_string_lossy();

    // Update the library path to include the x2t bin directory, fixes a bug where some of the requires
    // .so libraries aren't loaded when they need to be
    let ld_library_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let ld_library_path = format!("{}:{}", input.x2t_path.display(), ld_library_path);

    tracing::debug!("running x2t");

    let output = Command::new(x2t.as_ref())
        .arg(input.paths.config_path.display().to_string())
        .env("LD_LIBRARY_PATH", &ld_library_path)
        .output()
        .await
        .map_err(|err| {
            tracing::error!(?err, "failed to run x2t");
            ErrorResponse {
                reason: Some("RUN_X2T"),
                x2t_code: None,
                message: "failed to run x2t".to_string(),
            }
        })?;

    tracing::debug!("x2t complete");

    if !output.status.success() {
        let error_code = output.status.code();
        let message = error_code
            .and_then(get_error_code_message)
            .unwrap_or("unknown error occurred");

        let stderr = String::from_utf8_lossy(&output.stderr);

        tracing::debug!("reading file integrity");

        let mut file = tokio::fs::File::open(&input.paths.input_path)
            .await
            .map_err(|err| {
                tracing::error!(?err, "failed to open input file for integrity check");

                ErrorResponse {
                    reason: Some("OPEN_FILE_INTEGRITY"),
                    x2t_code: None,
                    message: "failed to open input file for integrity check".to_string(),
                }
            })?;
        let mut file_bytes = [0u8; 1024 * 32];
        let mut file_size: usize = 0;

        loop {
            // Read a chunk into the buffer
            let n = file
                .read(&mut file_bytes[file_size..])
                .await
                .map_err(|err| {
                    tracing::error!(?err, "failed to read input file for integrity check");

                    ErrorResponse {
                        reason: Some("READ_FILE_INTEGRITY"),
                        x2t_code: None,
                        message: "failed to read input file for integrity check".to_string(),
                    }
                })?;
            if n == 0 {
                break;
            }

            file_size += n;
        }

        file_size = file_size.min(file_bytes.len());

        tracing::debug!("finished reading file integrity, checking integrity");

        let file_condition = get_file_condition(&file_bytes[0..file_size]);

        tracing::error!(
            "error processing file (stderr = {stderr}, exit code = {error_code:?}, file_condition = {file_condition:?})"
        );

        // Assume encryption for out of range crashes
        if stderr.contains("std::out_of_range") {
            return Err(ErrorResponse {
                reason: Some("FILE_LIKELY_ENCRYPTED"),
                x2t_code: error_code,
                message: "file is encrypted".to_string(),
            });
        }

        return Err(match file_condition {
            FileCondition::LikelyCorrupted => ErrorResponse {
                reason: Some("FILE_LIKELY_CORRUPTED"),
                x2t_code: error_code,
                message: "file is corrupted".to_string(),
            },
            FileCondition::LikelyEncrypted => ErrorResponse {
                reason: Some("FILE_LIKELY_ENCRYPTED"),
                x2t_code: error_code,
                message: "file is encrypted".to_string(),
            },
            _ => ErrorResponse {
                reason: None,
                x2t_code: error_code,
                message: message.to_string(),
            },
        });
    }

    stream_output_file(
        input.s3_client,
        input.request.dest_bucket,
        input.request.dest_key,
        &input.paths.output_path,
    )
    .await?;

    Ok(())
}

#[derive(Deserialize)]
struct ConvertRequest {
    /// Bucket the input source file is within
    source_bucket: String,
    /// Key within the source bucket for the source file
    source_key: String,

    /// Bucket to store the output file
    dest_bucket: String,
    /// Key within the `dest_bucket` for the output file
    dest_key: String,
}

struct ConvertTempPaths {
    config_path: PathBuf,
    input_path: PathBuf,
    output_path: PathBuf,
}

/// Stream a file from S3 to disk
async fn stream_source_file(
    s3_client: &aws_sdk_s3::Client,
    source_bucket: String,
    source_key: String,
    file_path: &Path,
) -> Result<(), ErrorResponse> {
    let response = match s3_client
        .get_object()
        .bucket(source_bucket)
        .key(source_key)
        .send()
        .await
    {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(?err, "error streaming source file");

            if err
                .as_service_error()
                .is_some_and(|value| value.is_no_such_key())
            {
                return Err(ErrorResponse {
                    reason: Some("NO_SUCH_KEY"),
                    x2t_code: None,
                    message: "key not found in source bucket".to_string(),
                });
            }

            return Err(ErrorResponse {
                reason: Some("GET_OBJECT"),
                x2t_code: None,
                message: err.to_string(),
            });
        }
    };

    let mut body = response.body;

    let mut file = tokio::fs::File::create(file_path).await.map_err(|err| {
        tracing::error!(?err, "failed to create source file");
        ErrorResponse {
            reason: Some("GET_OBJECT"),
            x2t_code: None,
            message: err.to_string(),
        }
    })?;

    while let Some(chunk_result) = body.next().await {
        let chunk = chunk_result.map_err(|err| {
            tracing::error!(?err, "failed to read object chunk");
            ErrorResponse {
                reason: Some("READ_OBJECT_CHUNK"),
                x2t_code: None,
                message: "failed to read chunk".to_string(),
            }
        })?;

        file.write_all(&chunk).await.map_err(|err| {
            tracing::error!(?err, "failed to write object chunk");
            ErrorResponse {
                reason: Some("WRITE_OBJECT_CHUNK"),
                x2t_code: None,
                message: "failed to write chunk".to_string(),
            }
        })?;
    }

    file.flush().await.map_err(|err| {
        tracing::error!(?err, "failed to flush object");
        ErrorResponse {
            reason: Some("FLUSH_OBJECT"),
            x2t_code: None,
            message: "failed to flush object".to_string(),
        }
    })?;

    Ok(())
}

/// Stream a file upload from disk to S3
async fn stream_output_file(
    s3_client: &aws_sdk_s3::Client,
    dest_bucket: String,
    dest_key: String,
    file_path: &Path,
) -> Result<(), ErrorResponse> {
    let byte_stream = ByteStream::from_path(file_path).await.map_err(|err| {
        tracing::error!(?err, "failed to create output stream");
        ErrorResponse {
            reason: Some("CREATE_OUTPUT_STREAM"),
            x2t_code: None,
            message: "failed to create output stream".to_string(),
        }
    })?;

    s3_client
        .put_object()
        .bucket(dest_bucket)
        .key(dest_key)
        .body(byte_stream)
        .send()
        .await
        .map_err(|err| {
            tracing::error!(?err, "failed to upload output");
            ErrorResponse {
                reason: Some("UPLOAD_OUTPUT_STREAM"),
                x2t_code: None,
                message: "failed to upload output stream".to_string(),
            }
        })?;

    Ok(())
}

fn create_convert_temp_paths(temp_dir: &Path) -> std::io::Result<ConvertTempPaths> {
    // Generate random unique ID
    let random_id = Uuid::new_v4().simple();

    // Create paths in temp directory
    let config_path = temp_dir.join(format!("tmp_native_config_{random_id}.xml"));
    let input_path = temp_dir.join(format!("tmp_native_input_{random_id}"));
    let output_path = temp_dir.join(format!("tmp_native_output_{random_id}.pdf"));

    // Make paths absolute
    let config_path = absolute(config_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (config)"))?;
    let input_path = absolute(input_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (input)"))?;
    let output_path = absolute(output_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (output)"))?;

    Ok(ConvertTempPaths {
        config_path,
        input_path,
        output_path,
    })
}

/// Create the AWS production configuration
pub async fn aws_config() -> SdkConfig {
    let region_provider = RegionProviderChain::default_provider()
        // Fallback to our desired region
        .or_else("ap-southeast-2");

    // Load the configuration from env variables (See https://docs.aws.amazon.com/sdkref/latest/guide/settings-reference.html#EVarSettings)
    aws_config::defaults(BehaviorVersion::v2025_08_07())
        // Setup the region provider
        .region(region_provider)
        .load()
        .await
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub reason: Option<&'static str>,
    pub x2t_code: Option<i32>,
    pub message: String,
}

/// Translate a x2t error code to the common x2t error messages
fn get_error_code_message(code: i32) -> Option<&'static str> {
    Some(match code {
        0x0001 => "AVS_FILEUTILS_ERROR_UNKNOWN",
        0x0050 => "AVS_FILEUTILS_ERROR_CONVERT",
        0x0051 => "AVS_FILEUTILS_ERROR_CONVERT_DOWNLOAD",
        0x0052 => "AVS_FILEUTILS_ERROR_CONVERT_UNKNOWN_FORMAT",
        0x0053 => "AVS_FILEUTILS_ERROR_CONVERT_TIMEOUT",
        0x0054 => "AVS_FILEUTILS_ERROR_CONVERT_READ_FILE",
        0x0055 => "AVS_FILEUTILS_ERROR_CONVERT_DRM_UNSUPPORTED",
        0x0056 => "AVS_FILEUTILS_ERROR_CONVERT_CORRUPTED",
        0x0057 => "AVS_FILEUTILS_ERROR_CONVERT_LIBREOFFICE",
        0x0058 => "AVS_FILEUTILS_ERROR_CONVERT_PARAMS",
        0x0059 => "AVS_FILEUTILS_ERROR_CONVERT_NEED_PARAMS",
        0x005a => "AVS_FILEUTILS_ERROR_CONVERT_DRM",
        0x005b => "AVS_FILEUTILS_ERROR_CONVERT_PASSWORD",
        0x005c => "AVS_FILEUTILS_ERROR_CONVERT_ICU",
        0x005d => "AVS_FILEUTILS_ERROR_CONVERT_LIMITS",
        0x005e => "AVS_FILEUTILS_ERROR_CONVERT_ROWLIMITS",
        0x005f => "AVS_FILEUTILS_ERROR_CONVERT_DETECT",
        0x0060 => "AVS_FILEUTILS_ERROR_CONVERT_CELLLIMITS",
        _ => return None,
    })
}
