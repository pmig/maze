use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::time::Instant;

use aead_stream::{DecryptorBE32, NewStream, StreamBE32, StreamPrimitive};
use chacha20poly1305::aead::Payload;
use proptest::prelude::*;

use super::*;
use crate::{KEY_LEN, KeyId};

fn master(seed: u8) -> MasterKey {
    MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN])
}

fn cipher() -> BlobCipher {
    BlobCipher::new(&master(1))
}

fn id(seed: u8) -> BlobId {
    BlobId([seed; 32])
}

/// Deterministic, non-repeating test data.
fn data(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = 0x9e37_79b9_u32;
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn seal_streaming(cipher: &BlobCipher, plaintext: &[u8], blob_id: &BlobId, step: usize) -> Vec<u8> {
    let mut writer = cipher.seal_writer(blob_id, Vec::new()).unwrap();
    for piece in plaintext.chunks(step.max(1)) {
        writer.write_all(piece).unwrap();
    }
    writer.finish().unwrap()
}

fn open_streaming(
    cipher: &BlobCipher,
    sealed: &[u8],
    blob_id: &BlobId,
    step: usize,
) -> Result<Vec<u8>, Error> {
    let mut reader = cipher.open_reader(blob_id, Cursor::new(sealed))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; step.max(1)];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) => return Err(Error::Io(err)),
        }
    }
}

/// Opening must fail both one-shot and streaming.
#[track_caller]
fn assert_fails(cipher: &BlobCipher, sealed: &[u8], blob_id: &BlobId) {
    assert!(
        cipher.open(sealed, blob_id).is_err(),
        "one-shot open accepted bad data"
    );
    assert!(
        open_streaming(cipher, sealed, blob_id, 4096).is_err(),
        "streaming open accepted bad data"
    );
}

const EDGE_SIZES: &[usize] = &[
    0,
    1,
    2,
    100,
    CHUNK_LEN - 1,
    CHUNK_LEN,
    CHUNK_LEN + 1,
    2 * CHUNK_LEN - 1,
    2 * CHUNK_LEN,
    2 * CHUNK_LEN + 1,
];

#[test]
fn round_trips_edge_sizes() {
    let cipher = cipher();
    for &len in EDGE_SIZES {
        let plaintext = data(len);
        let sealed = cipher.seal(&plaintext, &id(1)).unwrap();
        assert_eq!(sealed.len() as u64, sealed_len(len as u64), "len {len}");
        assert_eq!(plaintext_len(sealed.len() as u64), Some(len as u64));
        assert_eq!(
            cipher.open(&sealed, &id(1)).unwrap(),
            plaintext,
            "len {len}"
        );
        assert_eq!(
            open_streaming(&cipher, &sealed, &id(1), 1000).unwrap(),
            plaintext
        );
    }
}

#[test]
fn round_trips_multi_megabyte() {
    let cipher = cipher();
    let plaintext = data(5 * 1024 * 1024 + 17);
    let sealed = cipher.seal(&plaintext, &id(2)).unwrap();
    assert_eq!(cipher.open(&sealed, &id(2)).unwrap(), plaintext);
    let streamed = seal_streaming(&cipher, &plaintext, &id(2), 100_003);
    assert_eq!(streamed.len(), sealed.len());
    assert_eq!(
        open_streaming(&cipher, &streamed, &id(2), 8192).unwrap(),
        plaintext
    );
    assert_eq!(cipher.open(&streamed, &id(2)).unwrap(), plaintext);
}

#[test]
fn empty_blob_is_header_and_one_tag() {
    let sealed = cipher().seal(&[], &id(1)).unwrap();
    assert_eq!(sealed.len(), HEADER_LEN + TAG_LEN);
    assert_eq!(&sealed[..4], b"MZB1");
    assert_eq!(sealed[4], VERSION);
    assert_eq!(&sealed[5..9], &[0, 0, 0, 1]);
    let streamed = seal_streaming(&cipher(), &[], &id(1), 1);
    assert_eq!(streamed.len(), HEADER_LEN + TAG_LEN);
    assert_eq!(cipher().open(&streamed, &id(1)).unwrap(), b"");
}

fn size_strategy() -> impl Strategy<Value = usize> {
    prop_oneof![
        prop::sample::select(EDGE_SIZES),
        0..4 * CHUNK_LEN,
        (1usize..=4).prop_map(|n| n * CHUNK_LEN),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn one_shot_round_trip(len in size_strategy(), blob in any::<[u8; 32]>()) {
        let cipher = cipher();
        let plaintext = data(len);
        let sealed = cipher.seal(&plaintext, &BlobId(blob)).unwrap();
        prop_assert_eq!(sealed.len() as u64, sealed_len(len as u64));
        prop_assert_eq!(cipher.open(&sealed, &BlobId(blob)).unwrap(), plaintext);
    }

    #[test]
    fn streaming_round_trip_with_any_io_sizes(
        len in size_strategy(),
        write_step in 1usize..200_000,
        read_step in 1usize..200_000,
    ) {
        let cipher = cipher();
        let plaintext = data(len);
        let sealed = seal_streaming(&cipher, &plaintext, &id(3), write_step);
        prop_assert_eq!(sealed.len() as u64, sealed_len(len as u64));
        prop_assert_eq!(
            &open_streaming(&cipher, &sealed, &id(3), read_step).unwrap(),
            &plaintext
        );
        prop_assert_eq!(&cipher.open(&sealed, &id(3)).unwrap(), &plaintext);
        let one_shot = cipher.seal(&plaintext, &id(3)).unwrap();
        prop_assert_eq!(open_streaming(&cipher, &one_shot, &id(3), read_step).unwrap(), plaintext);
    }

    #[test]
    fn length_helpers_agree(len in 0u64..(1 << 40)) {
        prop_assert_eq!(plaintext_len(sealed_len(len)), Some(len));
    }
}

#[test]
fn plaintext_len_rejects_impossible_sizes() {
    assert_eq!(plaintext_len(0), None);
    assert_eq!(plaintext_len((HEADER_LEN + TAG_LEN - 1) as u64), None);
    // A full chunk followed by a final chunk shorter than a tag.
    assert_eq!(
        plaintext_len((HEADER_LEN + SEALED_CHUNK_LEN + 3) as u64),
        None
    );
}

#[test]
fn every_tampered_byte_is_rejected() {
    let cipher = cipher();
    let sealed = cipher.seal(&data(300), &id(1)).unwrap();
    for position in 0..sealed.len() {
        for bit in [0x01, 0x80] {
            let mut bad = sealed.clone();
            bad[position] ^= bit;
            assert_fails(&cipher, &bad, &id(1));
        }
    }
}

#[test]
fn tampering_in_any_chunk_is_rejected() {
    let cipher = cipher();
    let sealed = cipher.seal(&data(3 * CHUNK_LEN + 10), &id(1)).unwrap();
    let positions = [
        HEADER_LEN,
        HEADER_LEN + CHUNK_LEN,                // first tag
        HEADER_LEN + SEALED_CHUNK_LEN,         // second chunk
        HEADER_LEN + 2 * SEALED_CHUNK_LEN + 5, // third chunk
        sealed.len() - 1,                      // final tag
    ];
    for position in positions {
        let mut bad = sealed.clone();
        bad[position] ^= 0x10;
        assert_fails(&cipher, &bad, &id(1));
    }
}

#[test]
fn wrong_key_is_rejected() {
    let sealed = cipher().seal(&data(1000), &id(1)).unwrap();
    assert_fails(&BlobCipher::new(&master(2)), &sealed, &id(1));
}

#[test]
fn wrong_blob_id_is_rejected() {
    let cipher = cipher();
    for len in [0, 10, CHUNK_LEN + 10] {
        let sealed = cipher.seal(&data(len), &id(1)).unwrap();
        assert_fails(&cipher, &sealed, &id(2));
    }
}

#[test]
fn other_key_generation_is_reported() {
    let newer = MasterKey::from_bytes(KeyId::new(2).unwrap(), [1; KEY_LEN]);
    let sealed = BlobCipher::new(&newer).seal(b"x", &id(1)).unwrap();
    assert_eq!(BlobHeader::parse(&sealed).unwrap().key_id().get(), 2);
    let err = cipher().open(&sealed, &id(1)).unwrap_err();
    assert!(
        matches!(err, Error::KeyIdMismatch { found: 2, .. }),
        "{err:?}"
    );
    assert!(matches!(
        cipher().open_reader(&id(1), Cursor::new(&sealed)),
        Err(Error::KeyIdMismatch { found: 2, .. })
    ));
}

#[test]
fn truncation_anywhere_is_rejected() {
    let cipher = cipher();
    for len in [0, 1, 1000, CHUNK_LEN, 2 * CHUNK_LEN, 2 * CHUNK_LEN + 100] {
        let sealed = cipher.seal(&data(len), &id(1)).unwrap();
        let mut cuts: Vec<usize> = vec![0, 3, 4, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + 1];
        cuts.extend((1..=3).map(|n| HEADER_LEN + n * SEALED_CHUNK_LEN));
        cuts.extend([sealed.len() - TAG_LEN, sealed.len() - 1]);
        for cut in cuts.into_iter().filter(|&cut| cut < sealed.len()) {
            assert_fails(&cipher, &sealed[..cut], &id(1));
        }
    }
}

#[test]
fn dropping_the_final_chunk_at_a_boundary_is_rejected() {
    let cipher = cipher();
    let sealed = cipher.seal(&data(2 * CHUNK_LEN + 100), &id(1)).unwrap();
    let cut = HEADER_LEN + 2 * SEALED_CHUNK_LEN;
    let err = cipher.open(&sealed[..cut], &id(1)).unwrap_err();
    assert!(matches!(err, Error::Decrypt), "{err:?}");
    assert_fails(&cipher, &sealed[..cut], &id(1));
}

#[test]
fn unfinished_writer_output_is_rejected() {
    let cipher = cipher();
    let mut writer = cipher.seal_writer(&id(1), Vec::new()).unwrap();
    writer.write_all(&data(3 * CHUNK_LEN)).unwrap();
    let partial = writer.get_ref().clone();
    drop(writer);
    assert_fails(&cipher, &partial, &id(1));
}

#[test]
fn trailing_data_is_rejected() {
    let cipher = cipher();
    for len in [0, 10, CHUNK_LEN, CHUNK_LEN + 10] {
        let sealed = cipher.seal(&data(len), &id(1)).unwrap();
        for extra in [1, TAG_LEN, SEALED_CHUNK_LEN] {
            let mut bad = sealed.clone();
            bad.extend(std::iter::repeat_n(0u8, extra));
            assert_fails(&cipher, &bad, &id(1));
        }
        // A second valid blob appended to the first.
        let mut doubled = sealed.clone();
        doubled.extend_from_slice(&sealed[HEADER_LEN..]);
        assert_fails(&cipher, &doubled, &id(1));
    }
}

#[test]
fn reordered_chunks_are_rejected() {
    let cipher = cipher();
    let sealed = cipher.seal(&data(3 * CHUNK_LEN), &id(1)).unwrap();
    let mut swapped = sealed.clone();
    let first = HEADER_LEN..HEADER_LEN + SEALED_CHUNK_LEN;
    let second = HEADER_LEN + SEALED_CHUNK_LEN..HEADER_LEN + 2 * SEALED_CHUNK_LEN;
    swapped[first.clone()].copy_from_slice(&sealed[second.clone()]);
    swapped[second].copy_from_slice(&sealed[first]);
    assert_fails(&cipher, &swapped, &id(1));
}

#[test]
fn chunks_from_another_blob_are_rejected() {
    let cipher = cipher();
    let a = cipher.seal(&data(2 * CHUNK_LEN), &id(1)).unwrap();
    let b = cipher.seal(&data(2 * CHUNK_LEN), &id(1)).unwrap();
    let mut spliced = a;
    spliced[HEADER_LEN..].copy_from_slice(&b[HEADER_LEN..]);
    assert_fails(&cipher, &spliced, &id(1));
}

#[test]
fn bad_magic_and_version_are_rejected() {
    let cipher = cipher();
    let sealed = cipher.seal(b"hello", &id(1)).unwrap();

    let mut bad_magic = sealed.clone();
    bad_magic[0] = b'X';
    assert!(matches!(
        cipher.open(&bad_magic, &id(1)),
        Err(Error::BadMagic)
    ));
    assert!(matches!(
        cipher.open_reader(&id(1), Cursor::new(&bad_magic)),
        Err(Error::BadMagic)
    ));

    let mut bad_version = sealed.clone();
    bad_version[4] = 2;
    assert!(matches!(
        cipher.open(&bad_version, &id(1)),
        Err(Error::UnsupportedVersion(2))
    ));

    let mut zero_key_id = sealed;
    zero_key_id[5..9].copy_from_slice(&[0; 4]);
    assert!(matches!(
        cipher.open(&zero_key_id, &id(1)),
        Err(Error::InvalidHeader(_))
    ));

    assert!(matches!(cipher.open(b"MZB", &id(1)), Err(Error::Truncated)));
    assert!(matches!(cipher.open(b"", &id(1)), Err(Error::Truncated)));
}

#[test]
fn reader_keeps_failing_after_an_error() {
    let cipher = cipher();
    let mut sealed = cipher.seal(&data(10), &id(1)).unwrap();
    let last = sealed.len() - 1;
    sealed[last] ^= 1;
    let mut reader = cipher.open_reader(&id(1), Cursor::new(&sealed)).unwrap();
    let mut buf = [0u8; 64];
    let err = reader.read(&mut buf).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(reader.read(&mut buf).is_err());
}

#[test]
fn nonces_are_unique_per_blob() {
    let cipher = cipher();
    let mut prefixes = HashSet::new();
    let mut ciphertexts = HashSet::new();
    for _ in 0..2000 {
        let sealed = cipher.seal(b"same plaintext", &id(1)).unwrap();
        assert!(prefixes.insert(sealed[9..HEADER_LEN].to_vec()));
        assert!(ciphertexts.insert(sealed[HEADER_LEN..].to_vec()));
    }
    // Streaming uses fresh nonces as well.
    let a = seal_streaming(&cipher, b"same", &id(1), 1);
    let b = seal_streaming(&cipher, b"same", &id(1), 1);
    assert_ne!(a[9..HEADER_LEN], b[9..HEADER_LEN]);
}

#[test]
fn compatible_with_aead_stream_be32() {
    let master = master(1);
    let key = master.derive(label::BLOB);
    let cipher = BlobCipher::new(&master);
    let plaintext = data(2 * CHUNK_LEN + 123);
    let sealed = cipher.seal(&plaintext, &id(7)).unwrap();

    let mut ad = sealed[..HEADER_LEN].to_vec();
    ad.extend_from_slice(&id(7).0);
    let prefix = &sealed[9..HEADER_LEN];
    let aead = XChaCha20Poly1305::new(key.expose_secret().into());
    let stream = StreamBE32::from_aead(aead, prefix.try_into().unwrap());
    let mut decryptor = DecryptorBE32::from_stream_primitive(stream);
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = sealed[HEADER_LEN..].chunks(SEALED_CHUNK_LEN).collect();
    let (last, rest) = chunks.split_last().unwrap();
    for chunk in rest {
        let payload = Payload {
            msg: chunk,
            aad: &ad,
        };
        out.extend(decryptor.decrypt_next(payload).unwrap());
    }
    let payload = Payload {
        msg: last,
        aad: &ad,
    };
    out.extend(decryptor.decrypt_last(payload).unwrap());
    assert_eq!(out, plaintext);

    // And the other direction: aead-stream output opens with BlobCipher.
    let aead = XChaCha20Poly1305::new(key.expose_secret().into());
    let header = BlobHeader::parse(&sealed).unwrap();
    let stream = StreamBE32::from_aead(aead, (&header.nonce_prefix[..]).try_into().unwrap());
    let mut theirs = header.to_bytes().to_vec();
    theirs.extend(
        stream
            .encrypt(
                0,
                true,
                Payload {
                    msg: b"hi",
                    aad: &ad,
                },
            )
            .unwrap(),
    );
    assert_eq!(cipher.open(&theirs, &id(7)).unwrap(), b"hi");
}

#[test]
fn debug_output_has_no_key_material() {
    let cipher = cipher();
    assert_eq!(format!("{cipher:?}"), "BlobCipher { key_id: KeyId(1), .. }");
}

/// Prints sealing and opening throughput. Run in release:
/// `cargo nextest run -p vault --release --run-ignored only throughput --no-capture`.
#[test]
#[ignore = "timing; run in release with --run-ignored"]
fn throughput() {
    let cipher = cipher();
    let plaintext = data(64 * 1024 * 1024);
    let started = Instant::now();
    let sealed = cipher.seal(&plaintext, &id(1)).unwrap();
    let seal_secs = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let opened = cipher.open(&sealed, &id(1)).unwrap();
    let open_secs = started.elapsed().as_secs_f64();
    assert_eq!(opened.len(), plaintext.len());
    let mb = plaintext.len() as f64 / 1e6;
    println!(
        "seal: {:.0} MB/s, open: {:.0} MB/s",
        mb / seal_secs,
        mb / open_secs
    );
    if !cfg!(debug_assertions) {
        assert!(mb / seal_secs >= 500.0, "sealing below 500 MB/s");
    }
}
