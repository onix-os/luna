use super::*;

#[derive(Copy, Clone)]
enum ExtendedBuffer {
    Indirect(*const [u8]),
    Inline {
        len: usize,
        #[cfg(target_pointer_width = "64")]
        table_hash: u64,
    },
}

#[derive(Copy, Clone)]
struct ExtendedHeader {
    hash: u64,
    buffer: ExtendedBuffer,
}

#[test]
fn inline_hash_word_fits_existing_header_and_allocation_layout() {
    fn copy_type<T: Copy>() {}
    copy_type::<StringInner>();
    copy_type::<ExtendedHeader>();
    assert_eq!(
        alloc::Layout::new::<ExtendedBuffer>(),
        alloc::Layout::new::<Buffer>()
    );
    assert_eq!(
        alloc::Layout::new::<ExtendedHeader>(),
        alloc::Layout::new::<StringInner>()
    );
    for len in [0, 1, 16, 256, 1024] {
        let bytes = alloc::Layout::array::<u8>(len).unwrap();
        assert_eq!(
            alloc::Layout::new::<ExtendedHeader>()
                .extend(bytes)
                .unwrap(),
            alloc::Layout::new::<StringInner>().extend(bytes).unwrap()
        );
        let header = ExtendedHeader {
            hash: 37,
            buffer: ExtendedBuffer::Inline {
                len,
                #[cfg(target_pointer_width = "64")]
                table_hash: u64::MAX,
            },
        };
        assert_eq!(header.hash, 37);
        let ExtendedBuffer::Inline { len: actual, .. } = header.buffer else {
            unreachable!()
        };
        assert_eq!(actual, len);
        #[cfg(target_pointer_width = "64")]
        if let ExtendedBuffer::Inline { table_hash, .. } = header.buffer {
            assert_eq!(table_hash, u64::MAX);
        }
    }
    let bytes = b"indirect";
    let ExtendedBuffer::Indirect(pointer) = ExtendedBuffer::Indirect(bytes.as_slice()) else {
        unreachable!()
    };
    assert!(std::ptr::eq(pointer, bytes.as_slice()));
}
