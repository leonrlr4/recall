//! Model files: where they live and how to fetch them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

/// Models the pipeline downloads on first use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelId {
    BreezeGgml,
    SileroVad,
    PyannoteSeg,
    CamPlusPlus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: ModelId,
    /// Path of the model relative to the models directory.
    pub file: &'static str,
    pub url: &'static str,
    /// Size of the download at `url` (for pyannote, the tarball), used for
    /// progress when the server sends no length.
    pub bytes: u64,
    /// Hex SHA-256 of the download at `url`, checked when set.
    pub sha256: Option<&'static str>,
}

/// The file to pull out of the pyannote tarball, which also holds an int8
/// variant, scripts and a licence.
const PYANNOTE_MEMBER: &str = "model.onnx";

/// Where `id` comes from and where it is stored.
pub fn spec(id: ModelId) -> ModelSpec {
    match id {
        // Breeze ASR 25, whisper.cpp q8_0 quantisation.
        ModelId::BreezeGgml => ModelSpec {
            id,
            file: "ggml-breeze-asr-25-q8_0.bin",
            url: "https://huggingface.co/shdennlin/breeze-asr-25-ggml/resolve/main/ggml-breeze-asr-25-q8_0.bin",
            bytes: 1_656_129_691,
            sha256: None,
        },
        ModelId::SileroVad => ModelSpec {
            id,
            file: "silero_vad.onnx",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
            bytes: 643_854,
            sha256: None,
        },
        // `sherpa-onnx-pyannote-segmentation-3-0.tar.bz2` holds
        // `sherpa-onnx-pyannote-segmentation-3-0/model.onnx` (5 992 913 bytes),
        // which is extracted to `file`.
        ModelId::PyannoteSeg => ModelSpec {
            id,
            file: "pyannote-segmentation-3-0/model.onnx",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2",
            bytes: 6_958_444,
            sha256: None,
        },
        // The release tag really is spelled "recongition".
        ModelId::CamPlusPlus => ModelSpec {
            id,
            file: "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
            bytes: 28_281_164,
            sha256: None,
        },
    }
}

/// The model file under `models_dir`, whether or not it exists yet.
pub fn path(models_dir: &Path, id: ModelId) -> PathBuf {
    models_dir.join(spec(id).file)
}

/// Returns the model's path, downloading it first if it is missing.
///
/// The download streams into a uniquely named `.<file>.*.part` temporary file
/// beside the model and is renamed into place only once complete, so an
/// interrupted download never leaves a file that looks finished, a failed
/// one deletes its temporary file, and concurrent callers each write their
/// own copy (the last rename wins, and every copy is whole). For pyannote the
/// tarball is the temporary download and `model.onnx` is extracted from it
/// into a second temporary file. `progress` receives `(downloaded, total)`
/// bytes after each chunk.
pub async fn ensure(
    models_dir: &Path,
    id: ModelId,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf> {
    ensure_spec(models_dir, &spec(id), progress).await
}

/// [`ensure`] for an explicit spec, so tests can point `url` at a local server.
async fn ensure_spec(
    models_dir: &Path,
    spec: &ModelSpec,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf> {
    let dest = models_dir.join(spec.file);
    if dest.is_file() {
        return Ok(dest);
    }
    let parent = dest.parent().unwrap_or(models_dir).to_owned();
    tokio::fs::create_dir_all(&parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let downloaded = download(spec, &parent, progress).await?;
    let model = if spec.id == ModelId::PyannoteSeg {
        let out = temp_file(&parent, spec.file)?;
        tokio::task::spawn_blocking(move || {
            // `downloaded` (the tarball) is deleted when this closure ends.
            extract_member(downloaded.path(), PYANNOTE_MEMBER, out.path())?;
            Ok::<_, anyhow::Error>(out)
        })
        .await
        .context("extracting pyannote model")??
    } else {
        downloaded
    };
    model
        .persist(&dest)
        .with_context(|| format!("moving download to {}", dest.display()))?;
    Ok(dest)
}

/// A temporary file in `dir` named after `file`, deleted on drop unless
/// persisted.
fn temp_file(dir: &Path, file: &str) -> Result<NamedTempFile> {
    let name = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".part")
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))
}

/// No total timeout: the Breeze model is 1.6 GB. A stalled connection fails
/// after 60 s without data instead.
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

/// Streams `spec.url` into a temporary file in `dir`, checking the length
/// against `Content-Length` and the SHA-256 when the spec has one.
async fn download(
    spec: &ModelSpec,
    dir: &Path,
    progress: impl Fn(u64, u64),
) -> Result<NamedTempFile> {
    let response = client()?
        .get(spec.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {}", spec.url))?;
    let expected_len = response.content_length();
    let total = expected_len.unwrap_or(spec.bytes);
    let tmp = temp_file(dir, spec.file)?;
    let write_err = || format!("writing {}", tmp.path().display());
    let mut file = tokio::fs::File::from_std(tmp.as_file().try_clone().with_context(write_err)?);
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("downloading {}", spec.url))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.with_context(write_err)?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.sync_all().await.with_context(write_err)?;
    drop(file);

    if let Some(expected) = expected_len
        && done != expected
    {
        bail!(
            "downloading {}: got {done} bytes, Content-Length says {expected}",
            spec.url
        );
    }
    if let Some(expected) = spec.sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("{} has SHA-256 {actual}, expected {expected}", spec.url);
        }
    }
    Ok(tmp)
}

/// Writes the first regular-file entry of the `.tar.bz2` at `tarball` whose
/// file name is `name` (in any directory) to `out`. Directories and links
/// with that name are skipped.
fn extract_member(tarball: &Path, name: &str, out: &Path) -> Result<()> {
    let file = fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(io::BufReader::new(file)));
    let entries = archive
        .entries()
        .with_context(|| format!("reading {}", tarball.display()))?;
    for entry in entries {
        let mut entry = entry.with_context(|| format!("reading {}", tarball.display()))?;
        let is_match = entry.header().entry_type().is_file()
            && entry.path()?.file_name().is_some_and(|n| n == name);
        if is_match {
            let mut dest =
                fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
            io::copy(&mut entry, &mut dest)
                .and_then(|_| dest.sync_all())
                .with_context(|| format!("writing {}", out.display()))?;
            return Ok(());
        }
    }
    Err(anyhow!("{} has no {name}", tarball.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_joins_models_dir_and_spec_file() {
        let dir = Path::new("/data/models");
        assert_eq!(path(dir, ModelId::SileroVad), dir.join("silero_vad.onnx"));
        assert_eq!(
            path(dir, ModelId::PyannoteSeg),
            dir.join("pyannote-segmentation-3-0/model.onnx")
        );
        for id in [
            ModelId::BreezeGgml,
            ModelId::SileroVad,
            ModelId::PyannoteSeg,
            ModelId::CamPlusPlus,
        ] {
            let s = spec(id);
            assert_eq!(s.id, id);
            assert!(s.url.starts_with("https://"), "{}", s.url);
            assert!(s.bytes > 0);
        }
    }

    #[tokio::test]
    async fn ensure_returns_existing_file_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let existing = path(dir.path(), ModelId::CamPlusPlus);
        std::fs::write(&existing, b"model").unwrap();
        let got = ensure(dir.path(), ModelId::CamPlusPlus, |_, _| {
            panic!("no download expected")
        })
        .await
        .unwrap();
        assert_eq!(got, existing);
    }

    #[test]
    fn extract_member_writes_only_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("pack.tar.bz2");
        {
            let file = std::fs::File::create(&tarball).unwrap();
            let enc = bzip2::write::BzEncoder::new(file, bzip2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            for (name, body) in [
                ("pack/model.int8.onnx", &b"int8"[..]),
                ("pack/model.onnx", &b"fp32"[..]),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, body).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("out/model.onnx");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        extract_member(&tarball, "model.onnx", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"fp32");

        let missing = extract_member(&tarball, "absent.onnx", &out).unwrap_err();
        assert!(format!("{missing:#}").contains("absent.onnx"));
    }

    /// Serves `body` to every connection on a local port, in 64 KiB writes
    /// with a pause between them so concurrent downloads interleave. The
    /// `Content-Length` header claims `claimed_len` bytes.
    async fn serve(body: Vec<u8>, claimed_len: usize) -> &'static str {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/model.onnx", listener.local_addr().unwrap());
        let body = std::sync::Arc::new(body);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {claimed_len}\r\nConnection: close\r\n\r\n"
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    for chunk in body.chunks(64 * 1024) {
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Box::leak(url.into_boxed_str())
    }

    fn test_spec(url: &'static str) -> ModelSpec {
        ModelSpec {
            id: ModelId::CamPlusPlus,
            file: "model.onnx",
            url,
            bytes: 1,
            sha256: None,
        }
    }

    fn body_1mb() -> Vec<u8> {
        (0..1_048_576u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ensure_concurrent_callers_produce_one_intact_file() {
        let body = body_1mb();
        let spec = test_spec(serve(body.clone(), body.len()).await);
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = tokio::join!(
            ensure_spec(dir.path(), &spec, |_, _| {}),
            ensure_spec(dir.path(), &spec, |_, _| {})
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a, dir.path().join("model.onnx"));
        assert_eq!(a, b);
        assert!(
            std::fs::read(&a).unwrap() == body,
            "downloaded bytes differ"
        );
        assert_eq!(dir_names(dir.path()), ["model.onnx"]);
    }

    #[tokio::test]
    async fn ensure_rejects_truncated_body() {
        let body = body_1mb();
        let spec = test_spec(serve(body.clone(), body.len() + 1_000).await);
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_spec(dir.path(), &spec, |_, _| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains(spec.url), "{err:#}");
        assert!(
            dir_names(dir.path()).is_empty(),
            "{:?}",
            dir_names(dir.path())
        );
    }

    #[test]
    fn extract_member_ignores_non_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("pack.tar.bz2");
        {
            let file = std::fs::File::create(&tarball).unwrap();
            let enc = bzip2::write::BzEncoder::new(file, bzip2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            // A directory and a symlink named like the member come first.
            let mut dir_header = tar::Header::new_gnu();
            dir_header.set_entry_type(tar::EntryType::Directory);
            dir_header.set_size(0);
            dir_header.set_mode(0o755);
            dir_header.set_cksum();
            builder
                .append_data(&mut dir_header, "a/model.onnx/", &b""[..])
                .unwrap();
            let mut link_header = tar::Header::new_gnu();
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_size(0);
            link_header.set_mode(0o777);
            builder
                .append_link(&mut link_header, "b/model.onnx", "/etc/passwd")
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "c/model.onnx", &b"fp32"[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("model.onnx");
        extract_member(&tarball, "model.onnx", &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"fp32");
    }
}
