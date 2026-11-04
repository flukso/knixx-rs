use super::*;
use std::vec::Vec;

const SEED: [u8; 32] = [7; 32];
const OTHER: [u8; 32] = [9; 32];

fn keys_of(seed: &[u8; 32], id: u8) -> [(u8, [u8; 32]); 1] {
    [(id, sign::public_key(seed))]
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + 3) as u8).collect()
}

fn kfw(body: &[u8], counter: u32) -> Vec<u8> {
    sign::kfw(
        Header::new(body, counter, "d36021e-dirty", 1_762_000_000),
        body,
        &SEED,
        1,
    )
}

fn verify(file: &[u8], keys: Keys, piece: usize) -> Result<Header, Refusal> {
    let hdr: &[u8; HEADER_SIZE] = file[..HEADER_SIZE].try_into().unwrap();
    let mut v = Verifier::new(hdr, keys)?;
    for p in file[HEADER_SIZE..].chunks(piece) {
        v.update(p);
    }
    v.finish()
}

#[test]
fn crc_known_values() {
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    let mut c = Crc32::new();
    c.update(b"1234");
    c.update(b"56789");
    assert_eq!(c.finish(), 0xCBF4_3926);
}

#[test]
fn header_round_trip() {
    let b = body(1000);
    let h = Header::new(&b, 42, "abc1234", 99);
    let raw = h.to_bytes();
    assert_eq!(&raw[..4], b"KNXX");
    let p = Header::parse(&raw).unwrap();
    assert_eq!(p, h);
    assert_eq!(p.version_str(), "abc1234");
    assert_eq!(p.file_size(), 1256);
}

#[test]
fn header_errors() {
    let h = Header::new(&body(10), 1, "x", 0);
    assert_eq!(Header::parse(&[0xFF; HEADER_SIZE]), Err(HeaderError::Magic));
    let mut raw = h.to_bytes();
    raw[30] ^= 1;
    assert_eq!(Header::parse(&raw), Err(HeaderError::Crc));
    let mut bad = h;
    bad.load = 0x26000;
    assert_eq!(Header::parse(&bad.to_bytes()), Err(HeaderError::Load(0x26000)));
    bad = h;
    bad.len = MAX_BODY + 1;
    assert_eq!(
        Header::parse(&bad.to_bytes()),
        Err(HeaderError::Len(MAX_BODY + 1))
    );
    bad.len = 0;
    assert_eq!(Header::parse(&bad.to_bytes()), Err(HeaderError::Len(0)));
}

#[test]
fn signed_image_verifies_in_any_pieces() {
    let b = body(10_000);
    let f = kfw(&b, 5);
    let keys = keys_of(&SEED, 1);
    for piece in [1, 7, 256, 4096, 20_000] {
        let h = verify(&f, Keys(&keys), piece).unwrap();
        assert_eq!(h.counter, 5);
        assert_eq!(h.key, 1);
    }
}

#[test]
fn tampering_is_refused() {
    let b = body(5000);
    let f = kfw(&b, 5);
    let keys = keys_of(&SEED, 1);

    // A flipped body byte: the CRC.
    let mut g = f.clone();
    g[HEADER_SIZE + 100] ^= 0x40;
    assert_eq!(verify(&g, Keys(&keys), 512), Err(Refusal::Crc));

    // A body changed with its CRC fixed in the header: the signature.
    let mut g = f.clone();
    g[HEADER_SIZE + 100] ^= 0x40;
    let mut h = Header::parse(g[..HEADER_SIZE].try_into().unwrap()).unwrap();
    h.body_crc = crc32(&g[HEADER_SIZE..]);
    g[..HEADER_SIZE].copy_from_slice(&h.to_bytes());
    assert_eq!(verify(&g, Keys(&keys), 512), Err(Refusal::Signature));

    // A raised counter: the signature.
    let mut h = Header::parse(f[..HEADER_SIZE].try_into().unwrap()).unwrap();
    h.counter += 1;
    let mut g = f.clone();
    g[..HEADER_SIZE].copy_from_slice(&h.to_bytes());
    assert_eq!(verify(&g, Keys(&keys), 512), Err(Refusal::Signature));

    // Short and long bodies.
    assert_eq!(verify(&f[..f.len() - 1], Keys(&keys), 512), Err(Refusal::Length));
    let mut g = f.clone();
    g.push(0);
    assert_eq!(verify(&g, Keys(&keys), 512), Err(Refusal::Length));

    // Another key, and an unknown key number.
    let other = keys_of(&OTHER, 1);
    assert_eq!(verify(&f, Keys(&other), 512), Err(Refusal::Signature));
    let wrong_id = keys_of(&SEED, 2);
    assert_eq!(verify(&f, Keys(&wrong_id), 512), Err(Refusal::UnknownKey(1)));
}

#[test]
fn several_keys() {
    let b = body(300);
    let f = kfw(&b, 1);
    let keys = [(0, sign::public_key(&OTHER)), (1, sign::public_key(&SEED))];
    assert!(verify(&f, Keys(&keys), 64).is_ok());
}

#[test]
fn counters() {
    let b = body(10);
    let running = Header::new(&b, 100, "a", 0);
    let newer = Header::new(&b, 101, "b", 0);
    let same = Header::new(&b, 100, "c", 0);
    assert_eq!(check_counter(&newer, Some(&running), false), Ok(()));
    assert_eq!(
        check_counter(&same, Some(&running), false),
        Err(Refusal::Downgrade {
            running: 100,
            image: 100
        })
    );
    assert_eq!(check_counter(&same, Some(&running), true), Ok(()));
    assert_eq!(check_counter(&same, None, false), Ok(()));
}

#[test]
fn sizes() {
    assert_eq!(check_size(MAX_IMAGE), Ok(()));
    assert_eq!(check_size(MAX_IMAGE + 1), Err(Refusal::TooLarge(MAX_IMAGE + 1)));
    assert_eq!(check_size(HEADER_SIZE as u32), Err(Refusal::Length));
    // The application region fits the cap.
    const { assert!(MAX_BODY + HEADER_SIZE as u32 <= MAX_IMAGE) };
}

#[test]
fn first_steps() {
    use Step::*;
    assert_eq!(first_step(0, false, true), Boot);
    assert_eq!(first_step(0, false, false), Install);
    assert_eq!(first_step(GPREGRET_INSTALL, false, true), Install);
    assert_eq!(first_step(GPREGRET_RECOVERY, false, true), Recovery(Why::Asked));
    assert_eq!(first_step(GPREGRET_INSTALL, true, true), Recovery(Why::DoubleTap));
    assert_eq!(first_step(0, true, false), Recovery(Why::DoubleTap));
    // An unknown value is no request.
    assert_eq!(first_step(0xA8, false, true), Boot);
}

#[test]
fn install_steps() {
    use InstallStep::*;
    let b = body(10);
    let a = Header::new(&b, 1, "a", 0);
    let n = Header::new(&b, 2, "b", 0);
    assert_eq!(install_step(Some(Ok(n)), Some(&a)), Copy);
    assert_eq!(install_step(Some(Ok(n)), None), Copy);
    assert_eq!(install_step(Some(Ok(a)), Some(&a)), Boot);
    assert_eq!(install_step(Some(Err(Refusal::Crc)), Some(&a)), Boot);
    assert_eq!(install_step(None, Some(&a)), Boot);
    assert_eq!(install_step(Some(Err(Refusal::Crc)), None), Recovery);
    assert_eq!(install_step(None, None), Recovery);
}

#[test]
fn layout() {
    // The vector table at APP_START needs 256-byte alignment (64 vectors).
    assert_eq!(APP_START % 256, 0);
    assert_eq!(APP_START, 0x0F100);
    assert_eq!(HEADER_ADDR % FLASH_PAGE, 0);
    assert_eq!(BOOT_START + BOOT_SIZE, HEADER_ADDR);
}
