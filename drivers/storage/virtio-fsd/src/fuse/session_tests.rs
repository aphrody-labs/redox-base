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
    let resp = FuseReply::<u32>::from_wire(&page, 20, 7, Data::None, None).unwrap();
    assert!(matches!(resp, FuseReply::Struct(42)));
}

#[test]
fn header_only_reply() {
    let page = page_with(header(16, 0, 3), &[]);
    let resp = FuseReply::<()>::from_wire(&page, 16, 3, Data::None, None).unwrap();
    assert!(matches!(resp, FuseReply::Struct(())));
}

#[test]
fn host_error_is_returned() {
    let page = page_with(header(16, -2, 3), &[]);
    let err = FuseReply::<u32>::from_wire(&page, 16, 3, Data::None, None).err();
    assert!(matches!(err, Some(FuseError::Host(2))));
}

#[test]
fn short_success_reply_is_rejected() {
    let page = page_with(header(16, 0, 3), &[]);
    assert!(FuseReply::<u32>::from_wire(&page, 16, 3, Data::None, None).is_err());
}

#[test]
fn nothing_written_is_rejected() {
    assert!(FuseReply::<()>::from_wire(&[0; 64], 0, 1, Data::None, None).is_err());
}

#[test]
#[should_panic(expected = "reply length '16' does not match bytes written '20'")]
fn len_mismatch_panics() {
    let page = page_with(header(16, 0, 3), &42u32.to_ne_bytes());
    let _ = FuseReply::<u32>::from_wire(&page, 20, 3, Data::None, None);
}

#[test]
#[should_panic(expected = "reply unique value '9' does not match request '3'")]
fn unique_mismatch_panics() {
    let page = page_with(header(16, 0, 9), &[]);
    let _ = FuseReply::<()>::from_wire(&page, 16, 3, Data::None, None);
}

#[test]
fn data_in_copies_what_was_written() {
    let page = page_with(header(20, 0, 4), &[]);
    let bounce = [1u8, 2, 3, 4];
    let mut buf = [0u8; 8];
    let resp = FuseReply::<()>::from_wire(&page, 20, 4, Data::In(&mut buf), Some(&bounce)).unwrap();
    assert!(matches!(resp, FuseReply::Size(4)));
    assert_eq!(&buf[..4], &bounce);
    assert_eq!(&buf[4..], &[0, 0, 0, 0]);
}

#[test]
fn data_in_with_nothing_to_copy() {
    let page = page_with(header(16, 0, 4), &[]);
    let mut buf = [7u8; 4];
    let resp = FuseReply::<()>::from_wire(&page, 16, 4, Data::In(&mut buf), None).unwrap();
    assert!(matches!(resp, FuseReply::Size(0)));
    assert_eq!(buf, [7; 4]);
}

#[test]
fn data_in_host_error_copies_nothing() {
    let page = page_with(header(16, -5, 4), &[]);
    let mut buf = [7u8; 4];
    let err = FuseReply::<()>::from_wire(&page, 16, 4, Data::In(&mut buf), None).err();
    assert!(matches!(err, Some(FuseError::Host(5))));
    assert_eq!(buf, [7; 4]);
}

#[test]
fn data_out_reply_carries_a_struct() {
    let page = page_with(header(20, 0, 8), &9u32.to_ne_bytes());
    let resp = FuseReply::<u32>::from_wire(&page, 20, 8, Data::Out(&[1, 2, 3]), None).unwrap();
    assert!(matches!(resp, FuseReply::Struct(9)));
}

#[test]
fn data_in_longer_than_buffer_is_rejected() {
    let page = page_with(header(26, 0, 4), &[]);
    let mut buf = [0u8; 4];
    assert!(
        FuseReply::<()>::from_wire(&page, 26, 4, Data::In(&mut buf), Some(&[0u8; 10])).is_err()
    );
}

#[test]
fn data_in_longer_than_bounce_is_rejected() {
    let page = page_with(header(26, 0, 4), &[]);
    let mut buf = [0u8; 16];
    assert!(FuseReply::<()>::from_wire(&page, 26, 4, Data::In(&mut buf), Some(&[0u8; 4])).is_err());
}
