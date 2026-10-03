use zerocopy::IntoBytes;

use super::*;

fn header(len: u32, error: i32, unique: u64) -> fuse_out_header {
    fuse_out_header { len, error, unique }
}

fn page_with(header: fuse_out_header, rest: &[u8]) -> Vec<u8> {
    let mut page = vec![0xA5; 64];
    let bytes = header.as_bytes();
    page[..bytes.len()].copy_from_slice(bytes);
    page[bytes.len()..bytes.len() + rest.len()].copy_from_slice(rest);
    page
}

#[test]
fn struct_reply() {
    let page = page_with(header(20, 0, 7), &42u32.to_ne_bytes());
    let resp = FuseResponse::<u32>::from_wire(&page, 20, 7, Data::None, None).unwrap();
    assert!(matches!(resp.payload, OutPayload::Struct(42)));
}

#[test]
fn header_only_reply() {
    let page = page_with(header(16, 0, 3), &[]);
    let resp = FuseResponse::<()>::from_wire(&page, 16, 3, Data::None, None).unwrap();
    assert!(matches!(resp.payload, OutPayload::None));
}

#[test]
fn host_error_is_header_only() {
    let page = page_with(header(16, -2, 3), &[]);
    let resp = FuseResponse::<u32>::from_wire(&page, 16, 3, Data::None, None).unwrap();
    assert_eq!(resp.fuse_out_header.error, -2);
    assert!(matches!(resp.payload, OutPayload::None));
}

#[test]
fn short_success_reply_is_rejected() {
    let page = page_with(header(16, 0, 3), &[]);
    assert!(FuseResponse::<u32>::from_wire(&page, 16, 3, Data::None, None).is_err());
}

#[test]
fn nothing_written_is_rejected() {
    assert!(FuseResponse::<()>::from_wire(&[0; 64], 0, 1, Data::None, None).is_err());
}

#[test]
fn len_mismatch_is_rejected() {
    let page = page_with(header(16, 0, 3), &42u32.to_ne_bytes());
    assert!(FuseResponse::<u32>::from_wire(&page, 20, 3, Data::None, None).is_err());
}

#[test]
fn unique_mismatch_is_rejected() {
    let page = page_with(header(16, 0, 9), &[]);
    assert!(FuseResponse::<()>::from_wire(&page, 16, 3, Data::None, None).is_err());
}

#[test]
fn data_in_copies_what_was_written() {
    let page = page_with(header(20, 0, 4), &[]);
    let bounce = [1u8, 2, 3, 4];
    let mut buf = [0u8; 8];
    let resp =
        FuseResponse::<()>::from_wire(&page, 20, 4, Data::In(&mut buf), Some(&bounce)).unwrap();
    assert!(matches!(resp.payload, OutPayload::Size(4)));
    assert_eq!(&buf[..4], &bounce);
    assert_eq!(&buf[4..], &[0, 0, 0, 0]);
}

#[test]
fn data_in_with_nothing_to_copy() {
    let page = page_with(header(16, 0, 4), &[]);
    let mut buf = [7u8; 4];
    let resp = FuseResponse::<()>::from_wire(&page, 16, 4, Data::In(&mut buf), None).unwrap();
    assert!(matches!(resp.payload, OutPayload::Size(0)));
    assert_eq!(buf, [7; 4]);
}

#[test]
fn data_in_host_error_copies_nothing() {
    let page = page_with(header(16, -5, 4), &[]);
    let mut buf = [7u8; 4];
    let resp = FuseResponse::<()>::from_wire(&page, 16, 4, Data::In(&mut buf), None).unwrap();
    assert_eq!(resp.fuse_out_header.error, -5);
    assert!(matches!(resp.payload, OutPayload::None));
    assert_eq!(buf, [7; 4]);
}

#[test]
fn data_out_reply_carries_a_struct() {
    let page = page_with(header(20, 0, 8), &9u32.to_ne_bytes());
    let resp = FuseResponse::<u32>::from_wire(&page, 20, 8, Data::Out(&[1, 2, 3]), None).unwrap();
    assert!(matches!(resp.payload, OutPayload::Struct(9)));
}

#[test]
fn data_in_longer_than_buffer_is_rejected() {
    let page = page_with(header(26, 0, 4), &[]);
    let mut buf = [0u8; 4];
    assert!(
        FuseResponse::<()>::from_wire(&page, 26, 4, Data::In(&mut buf), Some(&[0u8; 10])).is_err()
    );
}

#[test]
fn data_in_longer_than_bounce_is_rejected() {
    let page = page_with(header(26, 0, 4), &[]);
    let mut buf = [0u8; 16];
    assert!(
        FuseResponse::<()>::from_wire(&page, 26, 4, Data::In(&mut buf), Some(&[0u8; 4])).is_err()
    );
}
