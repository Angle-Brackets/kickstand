// SPDX-License-Identifier: GPL-3.0-only
//! `#[serde(with = "crate::bytes")]` for `Vec<u8>` fields: serde treats a `Vec<u8>` as a sequence and moves
//! it one byte at a time, which is most of the cost of decoding a 64 KiB chunk on the console. As bytes it's
//! one copy. postcard writes both the same way (a varint length, then the bytes), so the wire doesn't change.

use alloc::vec::Vec;
use core::fmt;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(data)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct Bytes;
    impl<'de> Visitor<'de> for Bytes {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("bytes")
        }
        fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }
        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(crate::MAX_FRAME));
            while let Some(b) = seq.next_element()? {
                out.push(b);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_byte_buf(Bytes)
}
