//! Model files: where they live and how to fetch them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
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
/// The model is written to `<file>.part` and renamed into place once
/// complete, so an interrupted download never leaves a file that looks
/// finished. For pyannote the tarball streams into `model.onnx.tar.bz2.part`
/// and `model.onnx` is extracted from it into the `.part` file. `progress`
/// receives `(downloaded, total)` bytes after each chunk.
pub async fn ensure(
    models_dir: &Path,
    id: ModelId,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf> {
    let spec = spec(id);
    let dest = path(models_dir, id);
    if dest.is_file() {
        return Ok(dest);
    }
    let parent = dest.parent().unwrap_or(models_dir);
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let part = part_path(&dest);
    if id == ModelId::PyannoteSeg {
        let tarball = part_path(&dest.with_extension("onnx.tar.bz2"));
        download(&spec, &tarball, progress).await?;
        let (from, to) = (tarball.clone(), part.clone());
        let result =
            tokio::task::spawn_blocking(move || extract_member(&from, PYANNOTE_MEMBER, &to))
                .await
                .context("extracting pyannote model")?;
        let _ = tokio::fs::remove_file(&tarball).await;
        result?;
    } else {
        download(&spec, &part, progress).await?;
    }
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    Ok(dest)
}

/// `<path>.part`.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Streams `spec.url` into `part`, checking the SHA-256 when the spec has one.
async fn download(spec: &ModelSpec, part: &Path, progress: impl Fn(u64, u64)) -> Result<()> {
    let response = reqwest::get(spec.url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {}", spec.url))?;
    let total = response.content_length().unwrap_or(spec.bytes);
    let mut file = tokio::fs::File::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("downloading {}", spec.url))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.sync_all()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    drop(file);

    if let Some(expected) = spec.sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(part).await;
            bail!("{} has SHA-256 {actual}, expected {expected}", spec.url);
        }
    }
    Ok(())
}

/// Writes the first entry of the `.tar.bz2` at `tarball` whose file name is
/// `name` (in any directory) to `out`.
fn extract_member(tarball: &Path, name: &str, out: &Path) -> Result<()> {
    let file = fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let mut archive = tar::Archive::new(bzip2::read::BzDecoder::new(io::BufReader::new(file)));
    let entries = archive
        .entries()
        .with_context(|| format!("reading {}", tarball.display()))?;
    for entry in entries {
        let mut entry = entry.with_context(|| format!("reading {}", tarball.display()))?;
        let path = entry.path()?;
        if path.file_name().is_some_and(|n| n == name) {
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
}
