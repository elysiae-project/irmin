// Sophon protobuf manifest
use bytes::Bytes;
use prost::Message;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serialize a `Bytes` field as a UTF-8 string for JSON persistence.
/// Used by `#[serde(serialize_with = "...")]` on `Bytes` fields that need
/// to survive JSON round-trips in `TargetSnapshot`/`TargetManifest`.
/// Returns a serialize error if the field contains invalid UTF-8, so a
/// corrupt or tampered manifest never silently persists as an empty string.
fn serialize_bytes_as_str<S: Serializer>(bytes: &Bytes, ser: S) -> Result<S::Ok, S::Error> {
    let s = std::str::from_utf8(bytes).map_err(serde::ser::Error::custom)?;
    ser.serialize_str(s)
}

/// Deserialize a `Bytes` field from a JSON string.
fn deserialize_bytes_from_str<'de, D: Deserializer<'de>>(de: D) -> Result<Bytes, D::Error> {
    let s = String::deserialize(de)?;
    Ok(Bytes::from(s))
}

/// Decoded full-install manifest containing the assets for a build.
///
/// Hot string fields use `bytes::Bytes` for zero-copy decode: prost stores
/// length-delimited fields as refcounted slices of the input buffer instead
/// of allocating per-field `String`s and copying the bytes. The wire format
/// is identical (both `string` and `bytes` use wire type 2). UTF-8 is
/// validated lazily on access via the accessor helpers below.
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub struct SophonManifestProto {
    #[prost(message, repeated, tag = "1")]
    pub assets: Vec<SophonManifestAssetProperty>,
}

/// Manifest record describing one file or directory in the game tree.
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub struct SophonManifestAssetProperty {
    /// Asset path relative to the game directory.
    #[prost(bytes, tag = "1")]
    #[serde(
        serialize_with = "serialize_bytes_as_str",
        deserialize_with = "deserialize_bytes_from_str"
    )]
    pub(crate) asset_name: Bytes,

    /// Chunks composing this asset.
    #[prost(message, repeated, tag = "2")]
    pub asset_chunks: Vec<SophonManifestAssetChunk>,

    /// 0 = file, 64 = directory.
    #[prost(uint32, tag = "3")]
    pub asset_type: u32,

    /// Uncompressed file size.
    #[prost(uint64, tag = "4")]
    pub asset_size: u64,

    /// MD5 of the assembled file.
    #[prost(bytes, tag = "5")]
    #[serde(
        serialize_with = "serialize_bytes_as_str",
        deserialize_with = "deserialize_bytes_from_str"
    )]
    pub(crate) asset_hash_md5: Bytes,
}

impl SophonManifestAssetProperty {
    /// Construct an asset property from its parts. Intended for benchmark
    /// and test fixtures; production code obtains these via protobuf decode.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        asset_name: Bytes,
        asset_chunks: Vec<SophonManifestAssetChunk>,
        asset_type: u32,
        asset_size: u64,
        asset_hash_md5: Bytes,
    ) -> Self {
        Self {
            asset_name,
            asset_chunks,
            asset_type,
            asset_size,
            asset_hash_md5,
        }
    }

    /// Returns true when the wire type is nonzero or the asset has no file hash.
    /// This accommodates directory records whose type/hash fields vary.
    #[inline]
    pub fn is_directory(&self) -> bool {
        self.asset_type != 0 || self.asset_hash_md5().is_empty()
    }

    /// Borrow the asset name as `&Bytes`.
    #[inline]
    pub fn asset_name(&self) -> &Bytes {
        &self.asset_name
    }

    /// Borrow the file MD5 as `&Bytes`.
    #[inline]
    pub fn asset_hash_md5(&self) -> &Bytes {
        &self.asset_hash_md5
    }

    /// Borrow the asset name as `&str`. UTF-8 validity is established at
    /// decode time by [`validate_manifest_utf8`].
    #[inline]
    pub fn asset_name_str(&self) -> &str {
        // SAFETY: `validate_manifest_utf8` rejects manifests with invalid
        // UTF-8 in `asset_name` at decode time, so every `Bytes` field that
        // survives decoding is valid UTF-8. `from_parts` callers in test
        // and benchmark code control their inputs, which are ASCII or hex.
        debug_assert!(std::str::from_utf8(&self.asset_name).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.asset_name) }
    }

    /// Borrow the file MD5 as `&str`. See [`asset_name_str`] for safety notes.
    #[inline]
    pub fn asset_hash_md5_str(&self) -> &str {
        // SAFETY: same invariant as `asset_name_str` — `asset_hash_md5` is
        // validated as UTF-8 at decode time by `validate_manifest_utf8`.
        debug_assert!(std::str::from_utf8(&self.asset_hash_md5).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.asset_hash_md5) }
    }
}

/// CDN object and file-range metadata for one asset chunk.
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub struct SophonManifestAssetChunk {
    /// CDN object name for the download URL.
    #[prost(bytes, tag = "1")]
    #[serde(
        serialize_with = "serialize_bytes_as_str",
        deserialize_with = "deserialize_bytes_from_str"
    )]
    pub(crate) chunk_name: Bytes,

    /// MD5 of the decompressed chunk.
    #[prost(bytes, tag = "2")]
    #[serde(
        serialize_with = "serialize_bytes_as_str",
        deserialize_with = "deserialize_bytes_from_str"
    )]
    pub(crate) chunk_decompressed_hash_md5: Bytes,

    /// Offset in the output file. Defaults to 0 for the first chunk.
    #[prost(uint64, tag = "3")]
    pub chunk_on_file_offset: u64,

    /// Compressed chunk size from the CDN.
    #[prost(uint64, tag = "4")]
    pub chunk_size: u64,

    /// Decompressed chunk size.
    #[prost(uint64, tag = "5")]
    pub chunk_size_decompressed: u64,

    /// Undocumented hash field. Not used for verification.
    #[prost(uint64, tag = "6")]
    pub chunk_compressed_hash_xxh: u64,

    /// MD5 of the compressed chunk from the CDN.
    #[prost(bytes, tag = "7")]
    #[serde(
        serialize_with = "serialize_bytes_as_str",
        deserialize_with = "deserialize_bytes_from_str"
    )]
    pub(crate) chunk_compressed_hash_md5: Bytes,

    /// Runtime-only: -1 = new data, >= 0 = offset in the old file for chunk
    /// reuse.
    #[prost(int64, tag = "8")]
    pub chunk_old_offset: i64,
}

impl SophonManifestAssetChunk {
    /// Construct a chunk from its parts. Intended for benchmark and test
    /// fixtures; production code obtains these via protobuf decode.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        chunk_name: Bytes,
        chunk_decompressed_hash_md5: Bytes,
        chunk_on_file_offset: u64,
        chunk_size: u64,
        chunk_size_decompressed: u64,
        chunk_compressed_hash_xxh: u64,
        chunk_compressed_hash_md5: Bytes,
        chunk_old_offset: i64,
    ) -> Self {
        Self {
            chunk_name,
            chunk_decompressed_hash_md5,
            chunk_on_file_offset,
            chunk_size,
            chunk_size_decompressed,
            chunk_compressed_hash_xxh,
            chunk_compressed_hash_md5,
            chunk_old_offset,
        }
    }

    /// Borrow the chunk name as `&Bytes`.
    #[inline]
    pub fn chunk_name(&self) -> &Bytes {
        &self.chunk_name
    }

    /// Borrow the decompressed MD5 as `&Bytes`.
    #[inline]
    pub fn chunk_decompressed_hash_md5(&self) -> &Bytes {
        &self.chunk_decompressed_hash_md5
    }

    /// Borrow the compressed MD5 as `&Bytes`.
    #[inline]
    pub fn chunk_compressed_hash_md5(&self) -> &Bytes {
        &self.chunk_compressed_hash_md5
    }

    /// Borrow the chunk name as `&str`. UTF-8 validity is established at
    /// decode time by [`validate_manifest_utf8`].
    #[inline]
    pub fn chunk_name_str(&self) -> &str {
        // SAFETY: `validate_manifest_utf8` rejects manifests with invalid
        // UTF-8 in `chunk_name` at decode time. `from_parts` callers
        // construct from ASCII/hex strings.
        debug_assert!(std::str::from_utf8(&self.chunk_name).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.chunk_name) }
    }

    /// Borrow the decompressed MD5 as `&str`.
    #[inline]
    pub fn chunk_decompressed_hash_md5_str(&self) -> &str {
        // SAFETY: same invariant as `chunk_name_str`.
        debug_assert!(std::str::from_utf8(&self.chunk_decompressed_hash_md5).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.chunk_decompressed_hash_md5) }
    }

    /// Borrow the compressed MD5 as `&str`.
    #[inline]
    pub fn chunk_compressed_hash_md5_str(&self) -> &str {
        // SAFETY: same invariant as `chunk_name_str`.
        debug_assert!(std::str::from_utf8(&self.chunk_compressed_hash_md5).is_ok());
        unsafe { std::str::from_utf8_unchecked(&self.chunk_compressed_hash_md5) }
    }
}

/// Normal manifests describe CDN data, not reusable offsets in an old installation.
/// Keep this separate from protobuf decoding, where omitted int64 fields remain zero.
pub(crate) fn normalize_full_download_manifest(manifest: &mut SophonManifestProto) {
    for asset in &mut manifest.assets {
        for chunk in &mut asset.asset_chunks {
            chunk.chunk_old_offset = -1;
        }
    }
}

/// Validate that every `Bytes` string field in the decoded manifest is valid
/// UTF-8. Returns a prost decode error on the first invalid field so a
/// corrupt or tampered CDN response is rejected at decode time rather than
/// silently producing empty strings from the `*_str()` accessor fallbacks.
#[allow(deprecated)]
fn validate_manifest_utf8(manifest: &SophonManifestProto) -> Result<(), prost::DecodeError> {
    for asset in &manifest.assets {
        std::str::from_utf8(&asset.asset_name)
            .map_err(|_| prost::DecodeError::new("asset_name is not valid UTF-8"))?;
        std::str::from_utf8(&asset.asset_hash_md5)
            .map_err(|_| prost::DecodeError::new("asset_hash_md5 is not valid UTF-8"))?;
        for chunk in &asset.asset_chunks {
            std::str::from_utf8(&chunk.chunk_name)
                .map_err(|_| prost::DecodeError::new("chunk_name is not valid UTF-8"))?;
            std::str::from_utf8(&chunk.chunk_decompressed_hash_md5).map_err(|_| {
                prost::DecodeError::new("chunk_decompressed_hash_md5 is not valid UTF-8")
            })?;
            std::str::from_utf8(&chunk.chunk_compressed_hash_md5).map_err(|_| {
                prost::DecodeError::new("chunk_compressed_hash_md5 is not valid UTF-8")
            })?;
        }
    }
    Ok(())
}

/// Decode a protobuf full manifest from any byte container.
///
/// Returns a prost decode error for malformed or truncated wire data, or
/// for string fields that contain invalid UTF-8. This does not validate
/// asset paths, hashes, or semantic field combinations.
///
/// For zero-copy decode, prefer [`decode_manifest_from_bytes`] which keeps the
/// input buffer alive via `Bytes` refcounts instead of copying each string field.
#[inline]
pub fn decode_manifest(buf: impl AsRef<[u8]>) -> Result<SophonManifestProto, prost::DecodeError> {
    let manifest = SophonManifestProto::decode(buf.as_ref())?;
    validate_manifest_utf8(&manifest)?;
    Ok(manifest)
}

/// Zero-copy decode: the returned manifest's `Bytes` fields hold refcounted
/// slices of `buf`, so `buf` must outlive the manifest (or be kept alive by
/// the `Bytes` refcounts themselves). Validates that all string fields are
/// valid UTF-8 before returning.
#[inline]
pub fn decode_manifest_from_bytes(buf: Bytes) -> Result<SophonManifestProto, prost::DecodeError> {
    let manifest = SophonManifestProto::decode(buf)?;
    validate_manifest_utf8(&manifest)?;
    Ok(manifest)
}

/// Decoded preinstall patch manifest, including patches and removals.
#[derive(Clone, PartialEq, Message)]
pub struct SophonPatchProto {
    #[prost(message, repeated, tag = "1")]
    pub patch_assets: Vec<SophonPatchAssetProperty>,

    #[prost(message, repeated, tag = "2")]
    pub unused_assets: Vec<SophonUnusedAssetProperty>,
}

/// Patch plan for one target asset and its candidate version-specific data.
#[derive(Clone, PartialEq, Message)]
pub struct SophonPatchAssetProperty {
    #[prost(string, tag = "1")]
    pub asset_name: String,

    #[prost(int64, tag = "2")]
    pub asset_size: i64,

    #[prost(string, tag = "3")]
    pub asset_hash_md5: String,

    #[prost(message, repeated, tag = "4")]
    pub asset_infos: Vec<SophonPatchAssetInfo>,
}

/// Version-tagged patch choice for a target asset; `chunk` may be absent.
#[derive(Clone, PartialEq, Message)]
pub struct SophonPatchAssetInfo {
    #[prost(string, tag = "1")]
    pub version_tag: String,

    #[prost(message, optional, tag = "2")]
    pub chunk: Option<SophonPatchAssetChunk>,
}

/// Patch payload range and the source-file identity required to apply it.
#[derive(Clone, PartialEq, Message)]
pub struct SophonPatchAssetChunk {
    #[prost(string, tag = "1")]
    pub patch_name: String,

    #[prost(string, tag = "2")]
    pub version_tag: String,

    #[prost(string, tag = "3")]
    pub build_id: String,

    #[prost(int64, tag = "4")]
    pub patch_size: i64,

    #[prost(string, tag = "5")]
    pub patch_md5: String,

    #[prost(int64, tag = "6")]
    pub patch_offset: i64,

    #[prost(int64, tag = "7")]
    pub patch_length: i64,

    #[prost(string, tag = "8")]
    pub original_file_name: String,

    #[prost(int64, tag = "9")]
    pub original_file_length: i64,

    #[prost(string, tag = "10")]
    pub original_file_md5: String,
}

/// Files no longer used by a given game version.
#[derive(Clone, PartialEq, Message)]
pub struct SophonUnusedAssetProperty {
    #[prost(string, tag = "1")]
    pub version_tag: String,

    #[prost(message, repeated, tag = "2")]
    pub asset_infos: Vec<SophonUnusedAssetInfo>,
}

/// One group of unused files associated with a version entry.
#[derive(Clone, PartialEq, Message)]
pub struct SophonUnusedAssetInfo {
    #[prost(message, repeated, tag = "1")]
    pub assets: Vec<SophonUnusedAssetFile>,
}

/// Path, expected size, and MD5 of a file to remove or validate as unused.
#[derive(Clone, PartialEq, Message)]
pub struct SophonUnusedAssetFile {
    #[prost(string, tag = "1")]
    pub file_name: String,

    #[prost(int64, tag = "2")]
    pub file_size: i64,

    #[prost(string, tag = "3")]
    pub file_md5: String,
}

/// Decode a protobuf patch manifest.
///
/// This validates wire-format decoding only; range, path, and checksum
/// validation occurs when the preinstall plan is built or applied.
#[inline]
pub fn decode_patch_manifest(
    buf: impl AsRef<[u8]>,
) -> Result<SophonPatchProto, prost::DecodeError> {
    SophonPatchProto::decode(buf.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_directory_type_nonzero_is_dir() {
        let prop = SophonManifestAssetProperty {
            asset_name: String::new().into(),
            asset_chunks: vec![],
            asset_type: 1,
            asset_size: 0,
            asset_hash_md5: bytes::Bytes::from("abc"),
        };
        assert!(prop.is_directory());
    }

    #[test]
    fn is_directory_type_64() {
        let prop = SophonManifestAssetProperty {
            asset_name: String::new().into(),
            asset_chunks: vec![],
            asset_type: 64,
            asset_size: 0,
            asset_hash_md5: bytes::Bytes::from("nonempty"),
        };
        assert!(prop.is_directory());
    }

    #[test]
    fn is_directory_empty_hash_is_dir() {
        let prop = SophonManifestAssetProperty {
            asset_name: String::new().into(),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: 0,
            asset_hash_md5: bytes::Bytes::new(),
        };
        assert!(prop.is_directory());
    }

    #[test]
    fn is_directory_is_file() {
        let prop = SophonManifestAssetProperty {
            asset_name: String::new().into(),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: 0,
            asset_hash_md5: bytes::Bytes::from("d41d8cd98f00b204e9800998ecf8427e"),
        };
        assert!(!prop.is_directory());
    }

    #[test]
    fn decode_manifest_empty_buf() {
        let result = decode_manifest([0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_encode_decode() {
        let chunk = SophonManifestAssetChunk {
            chunk_name: bytes::Bytes::from("chunk_001"),
            chunk_decompressed_hash_md5: bytes::Bytes::from("aabbccdd"),
            chunk_on_file_offset: 0,
            chunk_size: 1024,
            chunk_size_decompressed: 2048,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: bytes::Bytes::from("eeff0011"),
            chunk_old_offset: -1,
        };
        let original = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("GameData/Data.pak"),
                asset_chunks: vec![chunk],
                asset_type: 0,
                asset_size: 2048,
                asset_hash_md5: bytes::Bytes::from("11223344"),
            }],
        };

        let buf = original.encode_to_vec();
        let decoded: SophonManifestProto = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_patch_manifest_valid() {
        let chunk = SophonPatchAssetChunk {
            patch_name: "diff_001.hdiff".into(),
            version_tag: "v3.0.0".into(),
            build_id: "build_123".into(),
            patch_size: 4096,
            patch_md5: "aabbccdd".into(),
            patch_offset: 0,
            patch_length: 4096,
            original_file_name: "data.bin".into(),
            original_file_length: 8192,
            original_file_md5: "11223344".into(),
        };
        let asset = SophonPatchAssetProperty {
            asset_name: "GameData/data.bin".into(),
            asset_size: 8192,
            asset_hash_md5: "11223344".into(),
            asset_infos: vec![SophonPatchAssetInfo {
                version_tag: "v3.0.0".into(),
                chunk: Some(chunk),
            }],
        };
        let unused_file = SophonUnusedAssetFile {
            file_name: "old_data.bin".into(),
            file_size: 1024,
            file_md5: "deadbeef".into(),
        };
        let unused_info = SophonUnusedAssetInfo {
            assets: vec![unused_file],
        };
        let unused_asset = SophonUnusedAssetProperty {
            version_tag: "v2.0.0".into(),
            asset_infos: vec![unused_info],
        };
        let original = SophonPatchProto {
            patch_assets: vec![asset],
            unused_assets: vec![unused_asset],
        };

        let buf = original.encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_patch_manifest_invalid_garbage() {
        let result = decode_patch_manifest([0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_patch_manifest_empty() {
        let buf = SophonPatchProto {
            patch_assets: vec![],
            unused_assets: vec![],
        }
        .encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert!(decoded.patch_assets.is_empty());
        assert!(decoded.unused_assets.is_empty());
    }

    #[test]
    fn decode_patch_manifest_multiple_assets() {
        let make_asset = |name: &str, size: i64| SophonPatchAssetProperty {
            asset_name: name.into(),
            asset_size: size,
            asset_hash_md5: "hash".into(),
            asset_infos: vec![],
        };
        let original = SophonPatchProto {
            patch_assets: vec![
                make_asset("file_a.bin", 1000),
                make_asset("file_b.bin", 2000),
            ],
            unused_assets: vec![],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert_eq!(decoded.patch_assets.len(), 2);
        assert_eq!(decoded.patch_assets[0].asset_name, "file_a.bin");
        assert_eq!(decoded.patch_assets[1].asset_name, "file_b.bin");
    }

    #[test]
    fn decode_patch_manifest_large_values() {
        let chunk = SophonPatchAssetChunk {
            patch_name: "big.hdiff".into(),
            version_tag: "v1".into(),
            build_id: "b1".into(),
            patch_size: i64::MAX,
            patch_md5: "md5".into(),
            patch_offset: i64::MAX,
            patch_length: i64::MAX,
            original_file_name: "big.bin".into(),
            original_file_length: i64::MAX,
            original_file_md5: "md5".into(),
        };
        let asset = SophonPatchAssetProperty {
            asset_name: "big.bin".into(),
            asset_size: i64::MAX,
            asset_hash_md5: "hash".into(),
            asset_infos: vec![SophonPatchAssetInfo {
                version_tag: "v1".into(),
                chunk: Some(chunk),
            }],
        };
        let original = SophonPatchProto {
            patch_assets: vec![asset],
            unused_assets: vec![],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_patch_manifest_unused_only() {
        let unused = SophonUnusedAssetProperty {
            version_tag: "v1.0.0".into(),
            asset_infos: vec![SophonUnusedAssetInfo {
                assets: vec![SophonUnusedAssetFile {
                    file_name: "old.bin".into(),
                    file_size: 512,
                    file_md5: "abc".into(),
                }],
            }],
        };
        let original = SophonPatchProto {
            patch_assets: vec![],
            unused_assets: vec![unused],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_patch_manifest_empty_strings() {
        let chunk = SophonPatchAssetChunk {
            patch_name: String::new(),
            version_tag: String::new(),
            build_id: String::new(),
            patch_size: 0,
            patch_md5: String::new(),
            patch_offset: 0,
            patch_length: 0,
            original_file_name: String::new(),
            original_file_length: 0,
            original_file_md5: String::new(),
        };
        let asset = SophonPatchAssetProperty {
            asset_name: String::new(),
            asset_size: 0,
            asset_hash_md5: String::new(),
            asset_infos: vec![SophonPatchAssetInfo {
                version_tag: String::new(),
                chunk: Some(chunk),
            }],
        };
        let original = SophonPatchProto {
            patch_assets: vec![asset],
            unused_assets: vec![],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_patch_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn is_directory_type_0_nonempty_hash_is_not_dir() {
        let prop = SophonManifestAssetProperty {
            asset_name: bytes::Bytes::from("file.bin"),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: 1024,
            asset_hash_md5: bytes::Bytes::from("d41d8cd98f00b204e9800998ecf8427e"),
        };
        assert!(!prop.is_directory());
    }

    #[test]
    fn is_directory_type_0_empty_hash_is_dir() {
        let prop = SophonManifestAssetProperty {
            asset_name: bytes::Bytes::from("somedir"),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: 0,
            asset_hash_md5: bytes::Bytes::new(),
        };
        assert!(prop.is_directory());
    }

    #[test]
    fn decode_manifest_valid() {
        let original = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("Data/asset.pak"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("chunk_a"),
                    chunk_decompressed_hash_md5: bytes::Bytes::from("d1"),
                    chunk_on_file_offset: 0,
                    chunk_size: 500,
                    chunk_size_decompressed: 1000,
                    chunk_compressed_hash_xxh: 12345,
                    chunk_compressed_hash_md5: bytes::Bytes::from("c1"),
                    chunk_old_offset: -1,
                }],
                asset_type: 0,
                asset_size: 1000,
                asset_hash_md5: bytes::Bytes::from("a1"),
            }],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_manifest_multiple_assets() {
        let original = SophonManifestProto {
            assets: vec![
                SophonManifestAssetProperty {
                    asset_name: bytes::Bytes::from("file1.bin"),
                    asset_chunks: vec![],
                    asset_type: 0,
                    asset_size: 100,
                    asset_hash_md5: bytes::Bytes::from("h1"),
                },
                SophonManifestAssetProperty {
                    asset_name: bytes::Bytes::from("file2.bin"),
                    asset_chunks: vec![],
                    asset_type: 0,
                    asset_size: 200,
                    asset_hash_md5: bytes::Bytes::from("h2"),
                },
            ],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn decode_manifest_chunk_with_old_offset_zero() {
        let original = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("file.bin"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("chunk_x"),
                    chunk_decompressed_hash_md5: bytes::Bytes::new(),
                    chunk_on_file_offset: 0,
                    chunk_size: 1024,
                    chunk_size_decompressed: 2048,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: bytes::Bytes::new(),
                    chunk_old_offset: 0,
                }],
                asset_type: 0,
                asset_size: 2048,
                asset_hash_md5: bytes::Bytes::from("h"),
            }],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
        assert_eq!(decoded.assets[0].asset_chunks[0].chunk_old_offset, 0);
    }

    #[test]
    fn decode_manifest_chunk_with_positive_old_offset() {
        let original = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("file.bin"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("chunk_y"),
                    chunk_decompressed_hash_md5: bytes::Bytes::new(),
                    chunk_on_file_offset: 100,
                    chunk_size: 512,
                    chunk_size_decompressed: 1024,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: bytes::Bytes::new(),
                    chunk_old_offset: 200,
                }],
                asset_type: 0,
                asset_size: 2048,
                asset_hash_md5: bytes::Bytes::from("h"),
            }],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);
        assert_eq!(decoded.assets[0].asset_chunks[0].chunk_old_offset, 200);
    }

    /// Verifies u64::MAX and i64::MIN survive proto encode/decode without
    /// truncation. Guards against varint or field-type regressions.
    #[test]
    fn decode_manifest_boundary_values_survive_roundtrip() {
        let original = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("max_values.pak"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("boundary"),
                    chunk_decompressed_hash_md5: bytes::Bytes::from("x"),
                    chunk_on_file_offset: u64::MAX,
                    chunk_size: u64::MAX,
                    chunk_size_decompressed: u64::MAX,
                    chunk_compressed_hash_xxh: u64::MAX,
                    chunk_compressed_hash_md5: bytes::Bytes::from("y"),
                    chunk_old_offset: i64::MIN,
                }],
                asset_type: u32::MAX,
                asset_size: u64::MAX,
                asset_hash_md5: bytes::Bytes::from("z"),
            }],
        };
        let buf = original.encode_to_vec();
        let decoded = decode_manifest(&buf).unwrap();
        assert_eq!(original, decoded);

        let c = &decoded.assets[0].asset_chunks[0];
        assert_eq!(c.chunk_on_file_offset, u64::MAX);
        assert_eq!(c.chunk_size, u64::MAX);
        assert_eq!(c.chunk_size_decompressed, u64::MAX);
        assert_eq!(c.chunk_compressed_hash_xxh, u64::MAX);
        assert_eq!(c.chunk_old_offset, i64::MIN);
        assert_eq!(decoded.assets[0].asset_size, u64::MAX);
        assert_eq!(decoded.assets[0].asset_type, u32::MAX);
    }

    /// A manifest with invalid UTF-8 in any string field must be rejected
    /// by `decode_manifest` and `decode_manifest_from_bytes` so a corrupt
    /// or tampered CDN response cannot silently produce empty strings.
    #[test]
    fn decode_manifest_rejects_invalid_utf8_asset_name() {
        let mut chunk = SophonManifestAssetChunk {
            chunk_name: bytes::Bytes::from("ok"),
            chunk_decompressed_hash_md5: bytes::Bytes::from("ok"),
            chunk_on_file_offset: 0,
            chunk_size: 1,
            chunk_size_decompressed: 1,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: bytes::Bytes::from("ok"),
            chunk_old_offset: -1,
        };
        let asset = SophonManifestAssetProperty {
            asset_name: bytes::Bytes::from_static(b"\xff\xfe"),
            asset_chunks: vec![chunk.clone()],
            asset_type: 0,
            asset_size: 1,
            asset_hash_md5: bytes::Bytes::from("ok"),
        };
        let manifest = SophonManifestProto {
            assets: vec![asset],
        };
        let buf = manifest.encode_to_vec();
        assert!(
            decode_manifest(&buf).is_err(),
            "slice decode must reject bad UTF-8"
        );
        assert!(
            decode_manifest_from_bytes(bytes::Bytes::from(buf)).is_err(),
            "bytes decode must reject bad UTF-8"
        );

        // Also test invalid UTF-8 in a chunk field.
        chunk.chunk_name = bytes::Bytes::from_static(b"\xff\xfe");
        let asset = SophonManifestAssetProperty {
            asset_name: bytes::Bytes::from("ok"),
            asset_chunks: vec![chunk],
            asset_type: 0,
            asset_size: 1,
            asset_hash_md5: bytes::Bytes::from("ok"),
        };
        let manifest = SophonManifestProto {
            assets: vec![asset],
        };
        let buf = manifest.encode_to_vec();
        assert!(decode_manifest(&buf).is_err());
    }

    /// The custom serde helpers must round-trip a manifest with non-ASCII
    /// names through JSON without data loss.
    #[test]
    fn serde_bytes_round_trip_non_ascii() {
        let manifest = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("data/中文资产.pak"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("chunk_émoji_📦"),
                    chunk_decompressed_hash_md5: bytes::Bytes::from("aabbccdd"),
                    chunk_on_file_offset: 0,
                    chunk_size: 1024,
                    chunk_size_decompressed: 2048,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: bytes::Bytes::from("eeff0011"),
                    chunk_old_offset: -1,
                }],
                asset_type: 0,
                asset_size: 2048,
                asset_hash_md5: bytes::Bytes::from("d41d8cd98f00b204e9800998ecf8427e"),
            }],
        };
        let json = serde_json::to_string(&manifest).expect("serialize must succeed");
        let decoded: SophonManifestProto =
            serde_json::from_str(&json).expect("deserialize must succeed");
        assert_eq!(manifest, decoded);
    }

    /// `decode_manifest_from_bytes` and `decode_manifest` must produce
    /// identical results for the same input (both validate UTF-8).
    #[test]
    fn decode_bytes_and_slice_produce_identical_results() {
        let manifest = SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("test.bin"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("c0"),
                    chunk_decompressed_hash_md5: bytes::Bytes::from("dhash"),
                    chunk_on_file_offset: 0,
                    chunk_size: 10,
                    chunk_size_decompressed: 20,
                    chunk_compressed_hash_xxh: 42,
                    chunk_compressed_hash_md5: bytes::Bytes::from("chash"),
                    chunk_old_offset: -1,
                }],
                asset_type: 0,
                asset_size: 20,
                asset_hash_md5: bytes::Bytes::from("fhash"),
            }],
        };
        let buf = manifest.encode_to_vec();
        let via_slice = decode_manifest(&buf).unwrap();
        let via_bytes = decode_manifest_from_bytes(bytes::Bytes::from(buf)).unwrap();
        assert_eq!(via_slice, via_bytes);
    }
}
