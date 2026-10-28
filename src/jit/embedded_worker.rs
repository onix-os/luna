static IMAGE: Option<(&[u8], [u8; 32], &str)> = luna_jit_embed::image!();

pub(super) fn image() -> Option<(&'static [u8], &'static [u8; 32], &'static str)> {
    IMAGE
        .as_ref()
        .map(|(bytes, identity, target)| (*bytes, identity, *target))
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_descriptor_is_nonempty_target_matched_and_identity_bound() {
        let (bytes, identity, target) =
            super::image().expect("link builds must contain a real producer");
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..7], b"\x7fELF\x02\x01\x01");
        assert_eq!(
            *identity,
            crate::jit::compiler_request::image_identity(bytes, 512 << 20).unwrap()
        );
        assert!(matches!(
            target,
            "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" | "aarch64-unknown-linux-gnu"
        ));
    }
}
