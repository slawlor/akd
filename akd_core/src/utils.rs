// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

//! Utility functions

#[cfg(feature = "nostd")]
use alloc::vec::Vec;

/// Retrieve log_2 of the marker version, referring to the exponent
/// of the largest power of two that is at most the input version
pub fn get_marker_version_log2(version: u64) -> u64 {
    64 - (version.leading_zeros() as u64) - 1
}

/// Corresponds to the I2OSP() function from RFC8017, prepending the length of
/// a byte array to the byte array (so that it is ready for serialization and hashing)
///
/// Input byte array cannot be > 2^64-1 in length
pub fn i2osp_array(input: &[u8]) -> Vec<u8> {
    [&(input.len() as u64).to_be_bytes(), input].concat()
}

/// Serde serialization helpers
#[cfg(feature = "serde_serialization")]
pub mod serde_helpers {
    use hex::{FromHex, ToHex};
    use serde::Deserialize;

    /// A serde hex serializer for bytes
    pub fn bytes_serialize_hex<S, T>(x: &T, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
        T: AsRef<[u8]>,
    {
        let hex_str = &x.as_ref().encode_hex_upper::<String>();
        s.serialize_str(hex_str)
    }

    /// A serde hex deserializer for bytes
    pub fn bytes_deserialize_hex<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: AsRef<[u8]> + FromHex,
        <T as FromHex>::Error: core::fmt::Display,
    {
        let hex_str = String::deserialize(deserializer)?;
        T::from_hex(hex_str).map_err(serde::de::Error::custom)
    }

    /// Serialize a digest
    pub fn digest_serialize<S>(x: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde_bytes::Serialize;
        x.to_vec().serialize(s)
    }

    /// Deserialize a digest
    pub fn digest_deserialize<'de, D>(
        deserializer: D,
    ) -> Result<[u8; crate::hash::DIGEST_BYTES], D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let buf = <Vec<u8> as serde_bytes::Deserialize>::deserialize(deserializer)?;
        crate::hash::try_parse_digest(&buf).map_err(serde::de::Error::custom)
    }
}
