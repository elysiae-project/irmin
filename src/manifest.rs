//! Manifest content hashing for deterministic manifest identification.

use sha2::{Digest, Sha256};

use crate::proto_parse::SophonManifestProto;

/// Compute a deterministic content-based hash of a Sophon manifest.
///
/// Hashes the semantic content (not raw protobuf bytes) for stability across
/// API response variations. Assets are sorted by name; the undocumented
/// `chunk_compressed_hash_xxh` field is excluded. Returns SHA-256 truncated
/// to 8 hex chars.
/// Compute a stable content identity for the manifest's asset records.
///
/// The result is used to determine whether persisted chunk state belongs to the
/// same manifest; it is not a cryptographic signature or authenticity check.
pub fn compute_content_manifest_hash(manifest: &SophonManifestProto) -> String {
    let mut assets: Vec<_> = manifest.assets.iter().collect();
    assets.sort_unstable_by_key(|a| &a.asset_name);

    let mut hasher = Sha256::new();
    for asset in assets {
        hasher.update(asset.asset_name.as_ref());
        hasher.update(asset.asset_size.to_le_bytes());
        hasher.update(asset.asset_type.to_le_bytes());
        hasher.update(asset.asset_hash_md5.as_ref());
        for chunk in &asset.asset_chunks {
            hasher.update(chunk.chunk_name.as_ref());
            hasher.update(chunk.chunk_decompressed_hash_md5.as_ref());
            hasher.update(chunk.chunk_compressed_hash_md5.as_ref());
            hasher.update(chunk.chunk_on_file_offset.to_le_bytes());
            hasher.update(chunk.chunk_size.to_le_bytes());
            hasher.update(chunk.chunk_size_decompressed.to_le_bytes());
        }
    }
    hex::encode(&hasher.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use crate::manifest::compute_content_manifest_hash;
    use crate::proto_parse::{
        SophonManifestAssetChunk, SophonManifestAssetProperty, SophonManifestProto,
    };

    fn make_asset(name: &str, md5: &str, size: u64) -> SophonManifestAssetProperty {
        SophonManifestAssetProperty {
            asset_name: name.to_string().into(),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: size,
            asset_hash_md5: md5.to_string().into(),
        }
    }

    fn make_asset_with_chunks(
        name: &str,
        md5: &str,
        size: u64,
        xxh: u64,
    ) -> SophonManifestAssetProperty {
        SophonManifestAssetProperty {
            asset_name: name.to_string().into(),
            asset_chunks: vec![SophonManifestAssetChunk {
                chunk_name: bytes::Bytes::from("chunk_0"),
                chunk_decompressed_hash_md5: bytes::Bytes::from("decomp_md5"),
                chunk_on_file_offset: 0,
                chunk_size: size,
                chunk_size_decompressed: size,
                chunk_compressed_hash_xxh: xxh,
                chunk_compressed_hash_md5: bytes::Bytes::from("comp_md5"),
                chunk_old_offset: -1,
            }],
            asset_type: 0,
            asset_size: size,
            asset_hash_md5: md5.to_string().into(),
        }
    }

    #[test]
    fn compute_content_manifest_hash_deterministic() {
        let manifest = SophonManifestProto {
            assets: vec![
                make_asset("a.pak", "md5_a", 100),
                make_asset("b.pak", "md5_b", 200),
            ],
        };
        let h1 = compute_content_manifest_hash(&manifest);
        let h2 = compute_content_manifest_hash(&manifest);
        assert_eq!(h1, h2);
    }

    #[test]
    fn compute_content_manifest_hash_order_independent() {
        let manifest_ab = SophonManifestProto {
            assets: vec![
                make_asset("a.pak", "md5_a", 100),
                make_asset("b.pak", "md5_b", 200),
            ],
        };
        let manifest_ba = SophonManifestProto {
            assets: vec![
                make_asset("b.pak", "md5_b", 200),
                make_asset("a.pak", "md5_a", 100),
            ],
        };
        assert_eq!(
            compute_content_manifest_hash(&manifest_ab),
            compute_content_manifest_hash(&manifest_ba),
        );
    }

    #[test]
    fn compute_content_manifest_hash_different() {
        let manifest1 = SophonManifestProto {
            assets: vec![make_asset("a.pak", "md5_a", 100)],
        };
        let manifest2 = SophonManifestProto {
            assets: vec![make_asset("a.pak", "md5_different", 100)],
        };
        assert_ne!(
            compute_content_manifest_hash(&manifest1),
            compute_content_manifest_hash(&manifest2),
        );
    }

    #[test]
    fn compute_content_manifest_hash_empty() {
        let manifest = SophonManifestProto { assets: vec![] };
        let hash = compute_content_manifest_hash(&manifest);
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_content_manifest_hash_excludes_xxh() {
        let manifest_a = SophonManifestProto {
            assets: vec![make_asset_with_chunks("a.pak", "md5_a", 100, 111)],
        };
        let manifest_b = SophonManifestProto {
            assets: vec![make_asset_with_chunks("a.pak", "md5_a", 100, 999)],
        };
        assert_eq!(
            compute_content_manifest_hash(&manifest_a),
            compute_content_manifest_hash(&manifest_b),
        );
    }

    #[test]
    fn compute_content_manifest_hash_truncated() {
        let manifest = SophonManifestProto {
            assets: vec![make_asset("x.pak", "abc", 50)],
        };
        let hash = compute_content_manifest_hash(&manifest);
        assert_eq!(hash.len(), 16);
    }

    /// Golden value test. If this fails, the hash algorithm changed and all
    /// existing resume state files are invalidated.
    #[test]
    fn compute_content_manifest_hash_golden_value() {
        let manifest = SophonManifestProto {
            assets: vec![
                SophonManifestAssetProperty {
                    asset_name: bytes::Bytes::from("GameData/level01.pak"),
                    asset_chunks: vec![
                        SophonManifestAssetChunk {
                            chunk_name: bytes::Bytes::from("chunk_a1"),
                            chunk_decompressed_hash_md5: bytes::Bytes::from(
                                "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
                            ),
                            chunk_on_file_offset: 0,
                            chunk_size: 524288,
                            chunk_size_decompressed: 1048576,
                            chunk_compressed_hash_xxh: 12345,
                            chunk_compressed_hash_md5: bytes::Bytes::from(
                                "d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1",
                            ),
                            chunk_old_offset: -1,
                        },
                        SophonManifestAssetChunk {
                            chunk_name: bytes::Bytes::from("chunk_a2"),
                            chunk_decompressed_hash_md5: bytes::Bytes::from(
                                "1234567890abcdef1234567890abcdef",
                            ),
                            chunk_on_file_offset: 1048576,
                            chunk_size: 262144,
                            chunk_size_decompressed: 524288,
                            chunk_compressed_hash_xxh: 67890,
                            chunk_compressed_hash_md5: bytes::Bytes::from(
                                "fedcba0987654321fedcba0987654321",
                            ),
                            chunk_old_offset: -1,
                        },
                    ],
                    asset_type: 0,
                    asset_size: 1572864,
                    asset_hash_md5: bytes::Bytes::from("abcdef1234567890abcdef1234567890"),
                },
                SophonManifestAssetProperty {
                    asset_name: bytes::Bytes::from("GameData/config.json"),
                    asset_chunks: vec![SophonManifestAssetChunk {
                        chunk_name: bytes::Bytes::from("chunk_b1"),
                        chunk_decompressed_hash_md5: bytes::Bytes::from(
                            "00112233445566778899aabbccddeeff",
                        ),
                        chunk_on_file_offset: 0,
                        chunk_size: 4096,
                        chunk_size_decompressed: 8192,
                        chunk_compressed_hash_xxh: 11111,
                        chunk_compressed_hash_md5: bytes::Bytes::from(
                            "ffeeddccbbaa99887766554433221100",
                        ),
                        chunk_old_offset: -1,
                    }],
                    asset_type: 0,
                    asset_size: 8192,
                    asset_hash_md5: bytes::Bytes::from("11223344556677889900aabbccddeeff"),
                },
            ],
        };
        let hash = compute_content_manifest_hash(&manifest);
        assert_eq!(
            hash, "81909ab67f4a879a",
            "content hash algorithm changed: resume state compatibility broken"
        );
    }

    /// chunk_old_offset is runtime-only metadata and must not affect the hash.
    #[test]
    fn compute_content_manifest_hash_excludes_old_offset() {
        let make = |offset: i64| SophonManifestProto {
            assets: vec![SophonManifestAssetProperty {
                asset_name: bytes::Bytes::from("a.pak"),
                asset_chunks: vec![SophonManifestAssetChunk {
                    chunk_name: bytes::Bytes::from("c0"),
                    chunk_decompressed_hash_md5: bytes::Bytes::from("deadbeef"),
                    chunk_on_file_offset: 0,
                    chunk_size: 1000,
                    chunk_size_decompressed: 2000,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: bytes::Bytes::from("cafebabe"),
                    chunk_old_offset: offset,
                }],
                asset_type: 0,
                asset_size: 2000,
                asset_hash_md5: bytes::Bytes::from("filehash"),
            }],
        };
        assert_eq!(
            compute_content_manifest_hash(&make(-1)),
            compute_content_manifest_hash(&make(4096)),
        );
    }
}
