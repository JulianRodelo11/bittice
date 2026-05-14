use std::borrow::Cow;
use unicode_normalization::{is_nfc, UnicodeNormalization};

/// Returns the canonical byte representation of a primary key string.
///
/// Applies Unicode NFC normalization so that strings which are
/// canonically equivalent (precomposed vs decomposed forms of accented
/// characters, Hangul syllables, etc.) produce identical byte sequences.
///
/// CONTRACT: This function defines primary key identity in the storage
/// layer. Any change to its output for the same input requires a full
/// migration of every primary index on disk. Treat it as a stable wire
/// format.
///
/// - `Cow::Borrowed` for strings already in NFC avoids allocation in the
///   common case (ASCII and most real-world UTF-8).
/// - NFC, not NFKC: semantic differences are preserved (`①` ≠ `1`,
///   `ﬁ` ≠ `fi`). If compatible equivalence is needed later, that is a
///   separate function.
///
/// This function is not called from anywhere yet. It is the foundation
/// for the hash-based primary index refactor (Fase 1b+).
pub fn canonical_bytes(pk: &str) -> Cow<'_, [u8]> {
    if is_nfc(pk) {
        Cow::Borrowed(pk.as_bytes())
    } else {
        Cow::Owned(pk.nfc().collect::<String>().into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::borrow::Cow;

    // -----------------------------------------------------------------------
    // (a) Idempotencia
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn idempotent(s in ".*") {
            let first = canonical_bytes(&s);
            // Reconstruct the string from the canonical bytes (which are valid UTF-8)
            let as_str = std::str::from_utf8(&first).expect("canonical_bytes must be valid UTF-8");
            let second = canonical_bytes(as_str);
            prop_assert_eq!(first.as_ref(), second.as_ref());
        }
    }

    // -----------------------------------------------------------------------
    // (b) Equivalencia NFC
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn nfc_equivalence(s in ".*") {
            let nfc_string: String = s.nfc().collect();
            let a = canonical_bytes(&s);
            let b = canonical_bytes(&nfc_string);
            prop_assert_eq!(
                a.as_ref(),
                b.as_ref(),
                "canonical_bytes must agree with NFC-normalized input"
            );
        }
    }

    // -----------------------------------------------------------------------
    // (c) Pares NFC/NFD conocidos (casos de regresión)
    // -----------------------------------------------------------------------

    #[test]
    fn known_nfc_nfd_pairs() {
        // café: é precompuesto (U+00E9) vs descompuesto (e + U+0301)
        let cafe_nfc = "caf\u{00e9}";
        let cafe_nfd = "cafe\u{0301}";
        assert_eq!(
            canonical_bytes(cafe_nfc),
            canonical_bytes(cafe_nfd),
            "café NFC/NFD must produce identical bytes"
        );

        // ñ: precompuesto (U+00F1) vs descompuesto (n + U+0303)
        let n_tilde_nfc = "\u{00f1}";
        let n_tilde_nfd = "n\u{0303}";
        assert_eq!(
            canonical_bytes(n_tilde_nfc),
            canonical_bytes(n_tilde_nfd),
            "ñ NFC/NFD must produce identical bytes"
        );

        // 한: Hangul precompuesto (U+D55C) vs descompuesto (Jamos)
        let hangul_nfc = "\u{d55c}";
        let hangul_nfd = "\u{1112}\u{1161}\u{11ab}";
        assert_eq!(
            canonical_bytes(hangul_nfc),
            canonical_bytes(hangul_nfd),
            "한 NFC/NFD must produce identical bytes"
        );

        // ế: vocal vietnamita con tono — precompuesto vs descompuesto
        let viet_nfc = "\u{1ebf}";
        let viet_nfd = "e\u{0302}\u{0301}";
        assert_eq!(
            canonical_bytes(viet_nfc),
            canonical_bytes(viet_nfd),
            "ế NFC/NFD must produce identical bytes"
        );

        // Ω (Greek Omega U+03A9) vs Ω (Ohm sign U+2126)
        // U+2126 has a canonical decomposition mapping to U+03A9, so NFC
        // DOES unify them. This is correct NFC behavior — both represent
        // the same underlying character after canonical decomposition.
        let omega_letter = "\u{03a9}";
        let ohm_sign = "\u{2126}";
        assert_eq!(
            canonical_bytes(omega_letter),
            canonical_bytes(ohm_sign),
            "Ω (U+03A9) and Ω (U+2126) collapse under NFC via canonical decomposition"
        );

        // Guard against NFKC: compatibility-only equivalents must NOT collapse.
        // ﬁ (U+FB01, ligature fi) is only compatibility-equivalent to "fi",
        // not canonically equivalent. NFC must preserve the distinction.
        let fi_ligature = "\u{fb01}";
        let fi_plain = "fi";
        assert_ne!(
            canonical_bytes(fi_ligature),
            canonical_bytes(fi_plain),
            "ﬁ (U+FB01) must NOT collapse to 'fi' — that would be NFKC, not NFC"
        );

        // ① (U+2460, circled digit one) is only compatibility-equivalent to "1".
        let circled_one = "\u{2460}";
        let plain_one = "1";
        assert_ne!(
            canonical_bytes(circled_one),
            canonical_bytes(plain_one),
            "① (U+2460) must NOT collapse to '1' — that would be NFKC, not NFC"
        );
    }

    // -----------------------------------------------------------------------
    // (d) Inequivalencia
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn distinct_inputs_distinct_bytes(a in ".*", b in ".*") {
            let a_nfc: String = a.nfc().collect();
            let b_nfc: String = b.nfc().collect();
            prop_assume!(a_nfc != b_nfc);
            let ca = canonical_bytes(&a);
            let cb = canonical_bytes(&b);
            prop_assert_ne!(
                ca.as_ref(),
                cb.as_ref(),
                "distinct NFC-normalized strings must produce distinct bytes"
            );
        }
    }

    // -----------------------------------------------------------------------
    // (e) Empty string
    // -----------------------------------------------------------------------

    #[test]
    fn empty_string() {
        let result = canonical_bytes("");
        assert_eq!(result.as_ref(), b"");
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "empty string should return Borrowed"
        );
    }

    // -----------------------------------------------------------------------
    // (f) Sin alocación para ASCII
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn ascii_no_allocation(s in "[a-zA-Z0-9 _./:-]{0,1000}") {
            let result = canonical_bytes(&s);
            prop_assert!(
                matches!(result, Cow::Borrowed(_)),
                "ASCII-only string should return Cow::Borrowed, got Cow::Owned for {:?}",
                s
            );
        }
    }

    // -----------------------------------------------------------------------
    // (g) Strings largos (hasta 2KB)
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn long_strings(s in ".{0,2048}") {
            let result = canonical_bytes(&s);
            // No panic
            let bytes = result.as_ref();
            // Result is valid UTF-8
            prop_assert!(
                std::str::from_utf8(bytes).is_ok(),
                "canonical_bytes must return valid UTF-8"
            );
            // Idempotency holds for long strings too
            let as_str = std::str::from_utf8(bytes).unwrap();
            let second = canonical_bytes(as_str);
            prop_assert_eq!(
                bytes,
                second.as_ref(),
                "idempotency must hold for long strings"
            );
        }
    }
}
