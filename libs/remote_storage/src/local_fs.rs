//! Local filesystem acting as a remote storage.
//! Multiple API users can use the same "storage" of this kind by using different storage roots.
//!
//! This storage used in tests, but can also be used in cases when a certain persistent
//! volume is mounted to the local FS.

use std::{borrow::Cow, future::Future, io::ErrorKind, pin::Pin};

use anyhow::{bail, ensure, Context};
use bytes::Bytes;
use camino::{Utf8Path, Utf8PathBuf};
use futures::stream::Stream;
use tokio::{
    fs,
    io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;
use tracing::*;
use utils::{crashsafe::path_with_suffix_extension, fs_ext::is_directory_empty};

use crate::{Download, DownloadError, Listing, ListingMode, RemotePath};

use super::{RemoteStorage, StorageMetadata};

const LOCAL_FS_TEMP_FILE_SUFFIX: &str = "___temp";

#[derive(Debug, Clone)]
pub struct LocalFs {
    storage_root: Utf8PathBuf,
}

impl LocalFs {
    /// Attempts to create local FS storage, along with its root directory.
    /// Storage root will be created (if does not exist) and transformed into an absolute path (if passed as relative).
    pub fn new(mut storage_root: Utf8PathBuf) -> anyhow::Result<Self> {
        if !storage_root.exists() {
            std::fs::create_dir_all(&storage_root).with_context(|| {
                format!("Failed to create all directories in the given root path {storage_root:?}")
            })?;
        }
        if !storage_root.is_absolute() {
            storage_root = storage_root.canonicalize_utf8().with_context(|| {
                format!("Failed to represent path {storage_root:?} as an absolute path")
            })?;
        }

        Ok(Self { storage_root })
    }

    // mirrors S3Bucket::s3_object_to_relative_path
    fn local_file_to_relative_path(&self, key: Utf8PathBuf) -> RemotePath {
        let relative_path = key
            .strip_prefix(&self.storage_root)
            .expect("relative path must contain storage_root as prefix");
        RemotePath(relative_path.into())
    }

    async fn read_storage_metadata(
        &self,
        file_path: &Utf8Path,
    ) -> anyhow::Result<Option<StorageMetadata>> {
        let metadata_path = storage_metadata_path(file_path);
        if metadata_path.exists() && metadata_path.is_file() {
            let metadata_string = fs::read_to_string(&metadata_path).await.with_context(|| {
                format!("Failed to read metadata from the local storage at '{metadata_path}'")
            })?;

            serde_json::from_str(&metadata_string)
                .with_context(|| {
                    format!(
                        "Failed to deserialize metadata from the local storage at '{metadata_path}'",
                    )
                })
                .map(|metadata| Some(StorageMetadata(metadata)))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    async fn list_all(&self) -> anyhow::Result<Vec<RemotePath>> {
        Ok(get_all_files(&self.storage_root, true)
            .await?
            .into_iter()
            .map(|path| {
                path.strip_prefix(&self.storage_root)
                    .context("Failed to strip storage root prefix")
                    .and_then(RemotePath::new)
                    .expect(
                        "We list files for storage root, hence should be able to remote the prefix",
                    )
            })
            .collect())
    }

    // recursively lists all files in a directory,
    // mirroring the `list_files` for `s3_bucket`
    async fn list_recursive(&self, folder: Option<&RemotePath>) -> anyhow::Result<Vec<RemotePath>> {
        let full_path = match folder {
            Some(folder) => folder.with_base(&self.storage_root),
            None => self.storage_root.clone(),
        };

        // If we were given a directory, we may use it as our starting point.
        // Otherwise, we must go up to the first ancestor dir that exists.  This is because
        // S3 object list prefixes can be arbitrary strings, but when reading
        // the local filesystem we need a directory to start calling read_dir on.
        let mut initial_dir = full_path.clone();
        loop {
            // Did we make it to the root?
            if initial_dir.parent().is_none() {
                anyhow::bail!("list_files: failed to find valid ancestor dir for {full_path}");
            }

            match fs::metadata(initial_dir.clone()).await {
                Ok(meta) if meta.is_dir() => {
                    // We found a directory, break
                    break;
                }
                Ok(_meta) => {
                    // It's not a directory: strip back to the parent
                    initial_dir.pop();
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    // It's not a file that exists: strip the prefix back to the parent directory
                    initial_dir.pop();
                }
                Err(e) => {
                    // Unexpected I/O error
                    anyhow::bail!(e)
                }
            }
        }
        // Note that Utf8PathBuf starts_with only considers full path segments, but
        // object prefixes are arbitrary strings, so we need the strings for doing
        // starts_with later.
        let prefix = full_path.as_str();

        let mut files = vec![];
        let mut directory_queue = vec![initial_dir];
        while let Some(cur_folder) = directory_queue.pop() {
            let mut entries = cur_folder.read_dir_utf8()?;
            while let Some(Ok(entry)) = entries.next() {
                let file_name = entry.file_name();
                let full_file_name = cur_folder.join(file_name);
                if full_file_name.as_str().starts_with(prefix) {
                    let file_remote_path = self.local_file_to_relative_path(full_file_name.clone());
                    files.push(file_remote_path);
                    if full_file_name.is_dir() {
                        directory_queue.push(full_file_name);
                    }
                }
            }
        }

        Ok(files)
    }
}

#[async_trait::async_trait]
impl RemoteStorage for LocalFs {
    async fn list(
        &self,
        prefix: Option<&RemotePath>,
        mode: ListingMode,
    ) -> Result<Listing, DownloadError> {
        let mut result = Listing::default();

        if let ListingMode::NoDelimiter = mode {
            let keys = self
                .list_recursive(prefix)
                .await
                .map_err(DownloadError::Other)?;

            result.keys = keys
                .into_iter()
                .filter(|k| {
                    let path = k.with_base(&self.storage_root);
                    !path.is_dir()
                })
                .collect();

            return Ok(result);
        }

        let path = match prefix {
            Some(prefix) => Cow::Owned(prefix.with_base(&self.storage_root)),
            None => Cow::Borrowed(&self.storage_root),
        };

        let prefixes_to_filter = get_all_files(path.as_ref(), false)
            .await
            .map_err(DownloadError::Other)?;

        // filter out empty directories to mirror s3 behavior.
        for prefix in prefixes_to_filter {
            if prefix.is_dir()
                && is_directory_empty(&prefix)
                    .await
                    .map_err(DownloadError::Other)?
            {
                continue;
            }

            let stripped = prefix
                .strip_prefix(&self.storage_root)
                .context("Failed to strip prefix")
                .and_then(RemotePath::new)
                .expect(
                    "We list files for storage root, hence should be able to remote the prefix",
                );

            if prefix.is_dir() {
                result.prefixes.push(stripped);
            } else {
                result.keys.push(stripped);
            }
        }

        Ok(result)
    }

    async fn upload(
        &self,
        data: impl Stream<Item = std::io::Result<Bytes>> + Send + Sync,
        data_size_bytes: usize,
        to: &RemotePath,
        metadata: Option<StorageMetadata>,
    ) -> anyhow::Result<()> {
        let target_file_path = to.with_base(&self.storage_root);
        create_target_directory(&target_file_path).await?;
        // We need this dance with sort of durable rename (without fsyncs)
        // to prevent partial uploads. This was really hit when pageserver shutdown
        // cancelled the upload and partial file was left on the fs
        // NOTE: Because temp file suffix always the same this operation is racy.
        // Two concurrent operations can lead to the following sequence:
        // T1: write(temp)
        // T2: write(temp) -> overwrites the content
        // T1: rename(temp, dst) -> succeeds
        // T2: rename(temp, dst) -> fails, temp no longet exists
        // This can be solved by supplying unique temp suffix every time, but this situation
        // is not normal in the first place, the error can help (and helped at least once)
        // to discover bugs in upper level synchronization.
        let temp_file_path =
            path_with_suffix_extension(&target_file_path, LOCAL_FS_TEMP_FILE_SUFFIX);
        let mut destination = io::BufWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&temp_file_path)
                .await
                .with_context(|| {
                    format!("Failed to open target fs destination at '{target_file_path}'")
                })?,
        );

        let from_size_bytes = data_size_bytes as u64;
        let data = tokio_util::io::StreamReader::new(data);
        let data = std::pin::pin!(data);
        let mut buffer_to_read = data.take(from_size_bytes);

        // alternatively we could just write the bytes to a file, but local_fs is a testing utility
        let bytes_read = io::copy_buf(&mut buffer_to_read, &mut destination)
            .await
            .with_context(|| {
                format!(
                    "Failed to upload file (write temp) to the local storage at '{temp_file_path}'",
                )
            })?;

        if bytes_read < from_size_bytes {
            bail!("Provided stream was shorter than expected: {bytes_read} vs {from_size_bytes} bytes");
        }
        // Check if there is any extra data after the given size.
        let mut from = buffer_to_read.into_inner();
        let extra_read = from.read(&mut [1]).await?;
        ensure!(
            extra_read == 0,
            "Provided stream was larger than expected: expected {from_size_bytes} bytes",
        );

        destination.flush().await.with_context(|| {
            format!(
                "Failed to upload (flush temp) file to the local storage at '{temp_file_path}'",
            )
        })?;

        fs::rename(temp_file_path, &target_file_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to upload (rename) file to the local storage at '{target_file_path}'",
                )
            })?;

        if let Some(storage_metadata) = metadata {
            let storage_metadata_path = storage_metadata_path(&target_file_path);
            fs::write(
                &storage_metadata_path,
                serde_json::to_string(&storage_metadata.0)
                    .context("Failed to serialize storage metadata as json")?,
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to write metadata to the local storage at '{storage_metadata_path}'",
                )
            })?;
        }

        Ok(())
    }

    async fn download(&self, from: &RemotePath) -> Result<Download, DownloadError> {
        let target_path = from.with_base(&self.storage_root);
        if file_exists(&target_path).map_err(DownloadError::BadInput)? {
            let source = ReaderStream::new(
                fs::OpenOptions::new()
                    .read(true)
                    .open(&target_path)
                    .await
                    .with_context(|| {
                        format!("Failed to open source file {target_path:?} to use in the download")
                    })
                    .map_err(DownloadError::Other)?,
            );

            let metadata = self
                .read_storage_metadata(&target_path)
                .await
                .map_err(DownloadError::Other)?;
            Ok(Download {
                metadata,
                download_stream: Box::pin(source),
            })
        } else {
            Err(DownloadError::NotFound)
        }
    }

    async fn download_byte_range(
        &self,
        from: &RemotePath,
        start_inclusive: u64,
        end_exclusive: Option<u64>,
    ) -> Result<Download, DownloadError> {
        if let Some(end_exclusive) = end_exclusive {
            if end_exclusive <= start_inclusive {
                return Err(DownloadError::Other(anyhow::anyhow!("Invalid range, start ({start_inclusive}) is not less than end_exclusive ({end_exclusive:?})")));
            };
            if start_inclusive == end_exclusive.saturating_sub(1) {
                return Err(DownloadError::Other(anyhow::anyhow!("Invalid range, start ({start_inclusive}) and end_exclusive ({end_exclusive:?}) difference is zero bytes")));
            }
        }
        let target_path = from.with_base(&self.storage_root);
        if file_exists(&target_path).map_err(DownloadError::BadInput)? {
            let mut source = tokio::fs::OpenOptions::new()
                .read(true)
                .open(&target_path)
                .await
                .with_context(|| {
                    format!("Failed to open source file {target_path:?} to use in the download")
                })
                .map_err(DownloadError::Other)?;
            source
                .seek(io::SeekFrom::Start(start_inclusive))
                .await
                .context("Failed to seek to the range start in a local storage file")
                .map_err(DownloadError::Other)?;
            let metadata = self
                .read_storage_metadata(&target_path)
                .await
                .map_err(DownloadError::Other)?;

            Ok(match end_exclusive {
                Some(end_exclusive) => Download {
                    metadata,
                    download_stream: Box::pin(ReaderStream::new(
                        source.take(end_exclusive - start_inclusive),
                    )),
                },
                None => Download {
                    metadata,
                    download_stream: Box::pin(ReaderStream::new(source)),
                },
            })
        } else {
            Err(DownloadError::NotFound)
        }
    }

    async fn delete(&self, path: &RemotePath) -> anyhow::Result<()> {
        let file_path = path.with_base(&self.storage_root);
        match fs::remove_file(&file_path).await {
            Ok(()) => Ok(()),
            // The file doesn't exist. This shouldn't yield an error to mirror S3's behaviour.
            // See https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html
            // > If there isn't a null version, Amazon S3 does not remove any objects but will still respond that the command was successful.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    }

    async fn delete_objects<'a>(&self, paths: &'a [RemotePath]) -> anyhow::Result<()> {
        for path in paths {
            self.delete(path).await?
        }
        Ok(())
    }
}

fn storage_metadata_path(original_path: &Utf8Path) -> Utf8PathBuf {
    path_with_suffix_extension(original_path, "metadata")
}

fn get_all_files<'a, P>(
    directory_path: P,
    recursive: bool,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Utf8PathBuf>>> + Send + Sync + 'a>>
where
    P: AsRef<Utf8Path> + Send + Sync + 'a,
{
    Box::pin(async move {
        let directory_path = directory_path.as_ref();
        if directory_path.exists() {
            if directory_path.is_dir() {
                let mut paths = Vec::new();
                let mut dir_contents = fs::read_dir(directory_path).await?;
                while let Some(dir_entry) = dir_contents.next_entry().await? {
                    let file_type = dir_entry.file_type().await?;
                    let entry_path =
                        Utf8PathBuf::from_path_buf(dir_entry.path()).map_err(|pb| {
                            anyhow::Error::msg(format!(
                                "non-Unicode path: {}",
                                pb.to_string_lossy()
                            ))
                        })?;
                    if file_type.is_symlink() {
                        debug!("{entry_path:?} is a symlink, skipping")
                    } else if file_type.is_dir() {
                        if recursive {
                            paths.extend(get_all_files(&entry_path, true).await?.into_iter())
                        } else {
                            paths.push(entry_path)
                        }
                    } else {
                        paths.push(entry_path);
                    }
                }
                Ok(paths)
            } else {
                bail!("Path {directory_path:?} is not a directory")
            }
        } else {
            Ok(Vec::new())
        }
    })
}

async fn create_target_directory(target_file_path: &Utf8Path) -> anyhow::Result<()> {
    let target_dir = match target_file_path.parent() {
        Some(parent_dir) => parent_dir,
        None => bail!("File path '{target_file_path}' has no parent directory"),
    };
    if !target_dir.exists() {
        fs::create_dir_all(target_dir).await?;
    }
    Ok(())
}

fn file_exists(file_path: &Utf8Path) -> anyhow::Result<bool> {
    if file_path.exists() {
        ensure!(file_path.is_file(), "file path '{file_path}' is not a file");
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;

    use bytes::Bytes;
    use camino_tempfile::tempdir;
    use futures_util::Stream;
    use std::{collections::HashMap, io::Write};

    async fn read_and_assert_remote_file_contents(
        storage: &LocalFs,
        #[allow(clippy::ptr_arg)]
        // have to use &Utf8PathBuf due to `storage.local_path` parameter requirements
        remote_storage_path: &RemotePath,
        expected_metadata: Option<&StorageMetadata>,
    ) -> anyhow::Result<String> {
        let download = storage
            .download(remote_storage_path)
            .await
            .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
        ensure!(
            download.metadata.as_ref() == expected_metadata,
            "Unexpected metadata returned for the downloaded file"
        );

        let contents = aggregate(download.download_stream).await?;

        String::from_utf8(contents).map_err(anyhow::Error::new)
    }

    #[tokio::test]
    async fn upload_file() -> anyhow::Result<()> {
        let storage = create_storage()?;

        let target_path_1 = upload_dummy_file(&storage, "upload_1", None).await?;
        assert_eq!(
            storage.list_all().await?,
            vec![target_path_1.clone()],
            "Should list a single file after first upload"
        );

        let target_path_2 = upload_dummy_file(&storage, "upload_2", None).await?;
        assert_eq!(
            list_files_sorted(&storage).await?,
            vec![target_path_1.clone(), target_path_2.clone()],
            "Should list a two different files after second upload"
        );

        Ok(())
    }

    #[tokio::test]
    async fn upload_file_negatives() -> anyhow::Result<()> {
        let storage = create_storage()?;

        let id = RemotePath::new(Utf8Path::new("dummy"))?;
        let content = Bytes::from_static(b"12345");
        let content = move || futures::stream::once(futures::future::ready(Ok(content.clone())));

        // Check that you get an error if the size parameter doesn't match the actual
        // size of the stream.
        storage
            .upload(content(), 0, &id, None)
            .await
            .expect_err("upload with zero size succeeded");
        storage
            .upload(content(), 4, &id, None)
            .await
            .expect_err("upload with too short size succeeded");
        storage
            .upload(content(), 6, &id, None)
            .await
            .expect_err("upload with too large size succeeded");

        // Correct size is 5, this should succeed.
        storage.upload(content(), 5, &id, None).await?;

        Ok(())
    }

    fn create_storage() -> anyhow::Result<LocalFs> {
        let storage_root = tempdir()?.path().to_path_buf();
        LocalFs::new(storage_root)
    }

    #[tokio::test]
    async fn download_file() -> anyhow::Result<()> {
        let storage = create_storage()?;
        let upload_name = "upload_1";
        let upload_target = upload_dummy_file(&storage, upload_name, None).await?;

        let contents = read_and_assert_remote_file_contents(&storage, &upload_target, None).await?;
        assert_eq!(
            dummy_contents(upload_name),
            contents,
            "We should upload and download the same contents"
        );

        let non_existing_path = "somewhere/else";
        match storage.download(&RemotePath::new(Utf8Path::new(non_existing_path))?).await {
            Err(DownloadError::NotFound) => {} // Should get NotFound for non existing keys
            other => panic!("Should get a NotFound error when downloading non-existing storage files, but got: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn download_file_range_positive() -> anyhow::Result<()> {
        let storage = create_storage()?;
        let upload_name = "upload_1";
        let upload_target = upload_dummy_file(&storage, upload_name, None).await?;

        let full_range_download_contents =
            read_and_assert_remote_file_contents(&storage, &upload_target, None).await?;
        assert_eq!(
            dummy_contents(upload_name),
            full_range_download_contents,
            "Download full range should return the whole upload"
        );

        let uploaded_bytes = dummy_contents(upload_name).into_bytes();
        let (first_part_local, second_part_local) = uploaded_bytes.split_at(3);

        let first_part_download = storage
            .download_byte_range(&upload_target, 0, Some(first_part_local.len() as u64))
            .await?;
        assert!(
            first_part_download.metadata.is_none(),
            "No metadata should be returned for no metadata upload"
        );

        let first_part_remote = aggregate(first_part_download.download_stream).await?;
        assert_eq!(
            first_part_local, first_part_remote,
            "First part bytes should be returned when requested"
        );

        let second_part_download = storage
            .download_byte_range(
                &upload_target,
                first_part_local.len() as u64,
                Some((first_part_local.len() + second_part_local.len()) as u64),
            )
            .await?;
        assert!(
            second_part_download.metadata.is_none(),
            "No metadata should be returned for no metadata upload"
        );

        let second_part_remote = aggregate(second_part_download.download_stream).await?;
        assert_eq!(
            second_part_local, second_part_remote,
            "Second part bytes should be returned when requested"
        );

        Ok(())
    }

    #[tokio::test]
    async fn download_file_range_negative() -> anyhow::Result<()> {
        let storage = create_storage()?;
        let upload_name = "upload_1";
        let upload_target = upload_dummy_file(&storage, upload_name, None).await?;

        let start = 1_000_000_000;
        let end = start + 1;
        match storage
            .download_byte_range(
                &upload_target,
                start,
                Some(end), // exclusive end
            )
            .await
        {
            Ok(_) => panic!("Should not allow downloading wrong ranges"),
            Err(e) => {
                let error_string = e.to_string();
                assert!(error_string.contains("zero bytes"));
                assert!(error_string.contains(&start.to_string()));
                assert!(error_string.contains(&end.to_string()));
            }
        }

        let start = 10000;
        let end = 234;
        assert!(start > end, "Should test an incorrect range");
        match storage
            .download_byte_range(&upload_target, start, Some(end))
            .await
        {
            Ok(_) => panic!("Should not allow downloading wrong ranges"),
            Err(e) => {
                let error_string = e.to_string();
                assert!(error_string.contains("Invalid range"));
                assert!(error_string.contains(&start.to_string()));
                assert!(error_string.contains(&end.to_string()));
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn delete_file() -> anyhow::Result<()> {
        let storage = create_storage()?;
        let upload_name = "upload_1";
        let upload_target = upload_dummy_file(&storage, upload_name, None).await?;

        storage.delete(&upload_target).await?;
        assert!(storage.list_all().await?.is_empty());

        storage
            .delete(&upload_target)
            .await
            .expect("Should allow deleting non-existing storage files");

        Ok(())
    }

    #[tokio::test]
    async fn file_with_metadata() -> anyhow::Result<()> {
        let storage = create_storage()?;
        let upload_name = "upload_1";
        let metadata = StorageMetadata(HashMap::from([
            ("one".to_string(), "1".to_string()),
            ("two".to_string(), "2".to_string()),
        ]));
        let upload_target =
            upload_dummy_file(&storage, upload_name, Some(metadata.clone())).await?;

        let full_range_download_contents =
            read_and_assert_remote_file_contents(&storage, &upload_target, Some(&metadata)).await?;
        assert_eq!(
            dummy_contents(upload_name),
            full_range_download_contents,
            "We should upload and download the same contents"
        );

        let uploaded_bytes = dummy_contents(upload_name).into_bytes();
        let (first_part_local, _) = uploaded_bytes.split_at(3);

        let partial_download_with_metadata = storage
            .download_byte_range(&upload_target, 0, Some(first_part_local.len() as u64))
            .await?;
        let first_part_remote = aggregate(partial_download_with_metadata.download_stream).await?;
        assert_eq!(
            first_part_local,
            first_part_remote.as_slice(),
            "First part bytes should be returned when requested"
        );

        assert_eq!(
            partial_download_with_metadata.metadata,
            Some(metadata),
            "We should get the same metadata back for partial download"
        );

        Ok(())
    }

    #[tokio::test]
    async fn list() -> anyhow::Result<()> {
        // No delimiter: should recursively list everything
        let storage = create_storage()?;
        let child = upload_dummy_file(&storage, "grandparent/parent/child", None).await?;
        let uncle = upload_dummy_file(&storage, "grandparent/uncle", None).await?;

        let listing = storage.list(None, ListingMode::NoDelimiter).await?;
        assert!(listing.prefixes.is_empty());
        assert_eq!(listing.keys, [uncle.clone(), child.clone()].to_vec());

        // Delimiter: should only go one deep
        let listing = storage.list(None, ListingMode::WithDelimiter).await?;

        assert_eq!(
            listing.prefixes,
            [RemotePath::from_string("timelines").unwrap()].to_vec()
        );
        assert!(listing.keys.is_empty());

        // Delimiter & prefix
        let listing = storage
            .list(
                Some(&RemotePath::from_string("timelines/some_timeline/grandparent").unwrap()),
                ListingMode::WithDelimiter,
            )
            .await?;
        assert_eq!(
            listing.prefixes,
            [RemotePath::from_string("timelines/some_timeline/grandparent/parent").unwrap()]
                .to_vec()
        );
        assert_eq!(listing.keys, [uncle.clone()].to_vec());

        Ok(())
    }

    async fn upload_dummy_file(
        storage: &LocalFs,
        name: &str,
        metadata: Option<StorageMetadata>,
    ) -> anyhow::Result<RemotePath> {
        let from_path = storage
            .storage_root
            .join("timelines")
            .join("some_timeline")
            .join(name);
        let (file, size) = create_file_for_upload(&from_path, &dummy_contents(name)).await?;

        let relative_path = from_path
            .strip_prefix(&storage.storage_root)
            .context("Failed to strip storage root prefix")
            .and_then(RemotePath::new)
            .with_context(|| {
                format!(
                    "Failed to resolve remote part of path {:?} for base {:?}",
                    from_path, storage.storage_root
                )
            })?;

        let file = tokio_util::io::ReaderStream::new(file);

        storage.upload(file, size, &relative_path, metadata).await?;
        Ok(relative_path)
    }

    async fn create_file_for_upload(
        path: &Utf8Path,
        contents: &str,
    ) -> anyhow::Result<(fs::File, usize)> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut file_for_writing = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        write!(file_for_writing, "{}", contents)?;
        drop(file_for_writing);
        let file_size = path.metadata()?.len() as usize;
        Ok((
            fs::OpenOptions::new().read(true).open(&path).await?,
            file_size,
        ))
    }

    fn dummy_contents(name: &str) -> String {
        format!("contents for {name}")
    }

    async fn list_files_sorted(storage: &LocalFs) -> anyhow::Result<Vec<RemotePath>> {
        let mut files = storage.list_all().await?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(files)
    }

    async fn aggregate(
        stream: impl Stream<Item = std::io::Result<Bytes>>,
    ) -> anyhow::Result<Vec<u8>> {
        use futures::stream::StreamExt;
        let mut out = Vec::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(res) = stream.next().await {
            out.extend_from_slice(&res?[..]);
        }
        Ok(out)
    }
}
