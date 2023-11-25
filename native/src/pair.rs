use std::io::Write;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const EXPORTED_KEY_LABEL: &str = "adb-label\u{0}";
pub const CLIENT_NAME: &str = "adb pair client\u{0}";
pub const SERVER_NAME: &str = "adb pair server\u{0}";
pub const MAX_PEER_INFO_SIZE: i32 = 1 << 13;

const ANDROID_PUBKEY_MODULUS_SIZE: i32 = 2048 / 8;
const ANDROID_PUBKEY_ENCODED_SIZE: i32 = 3 * 4 + 2 * ANDROID_PUBKEY_MODULUS_SIZE;
const ANDROID_PUBKEY_MODULUS_SIZE_WORDS: i32 = ANDROID_PUBKEY_MODULUS_SIZE / 4;

fn generate_cert() -> anyhow::Result<(boring::x509::X509, boring::pkey::PKey<boring::pkey::Private>)> {
    let rsa = boring::rsa::Rsa::generate(2048).context("failed to generate rsa keypair")?;
    // put it into the pkey struct
    let pkey = boring::pkey::PKey::from_rsa(rsa).context("failed to create pkey struct from rsa keypair")?;

    // make a new x509 certificate with the pkey we generated
    let mut x509builder = boring::x509::X509::builder().context("failed to make x509 builder")?;
    x509builder
        .set_version(2)
        .context("failed to set x509 version")?;

    // set the serial number to some big random positive integer
    let mut serial = boring::bn::BigNum::new().context("failed to make new bignum")?;
    serial
        .rand(32, boring::bn::MsbOption::ONE, false)
        .context("failed to generate random bignum")?;
    let serial = serial
        .to_asn1_integer()
        .context("failed to get asn1 integer from bignum")?;
    x509builder
        .set_serial_number(&serial)
        .context("failed to set x509 serial number")?;

    // call fails without expiration dates
    // I guess they are important anyway, but still
    let not_before = boring::asn1::Asn1Time::days_from_now(0).context("failed to parse 'notBefore' timestamp")?;
    let not_after = boring::asn1::Asn1Time::days_from_now(360)
        .context("failed to parse 'notAfter' timestamp")?;
    x509builder
        .set_not_before(&not_before)
        .context("failed to set x509 start date")?;
    x509builder
        .set_not_after(&not_after)
        .context("failed to set x509 expiration date")?;

    // add the issuer and subject name
    // it's set to "/CN=LinuxTransport"
    // if we want we can make that configurable later
    let mut x509namebuilder = boring::x509::X509Name::builder().context("failed to get x509name builder")?;
    x509namebuilder
        .append_entry_by_text("CN", "LinuxTransport")
        .context("failed to append /CN=LinuxTransport to x509name builder")?;
    let x509name = x509namebuilder.build();
    x509builder
        .set_issuer_name(&x509name)
        .context("failed to set x509 issuer name")?;
    x509builder
        .set_subject_name(&x509name)
        .context("failed to set x509 subject name")?;

    // set the public key
    x509builder
        .set_pubkey(&pkey)
        .context("failed to set x509 pubkey")?;

    // it also needs several extensions
    // in the openssl configuration file, these are set when generating certs
    //     basicConstraints=CA:true
    //     subjectKeyIdentifier=hash
    //     authorityKeyIdentifier=keyid:always,issuer
    // that means these extensions get added to certs generated using the
    // command line tool automatically. but since we are constructing it, we
    // need to add them manually.
    // we need to do them one at a time, and they need to be in this order
    // let conf = boring::conf::Conf::new(boring::conf::ConfMethod::).context("failed to make new conf struct")?;
    // it seems like everything depends on the basic constraints, so let's do
    // that first.
    let bc = boring::x509::extension::BasicConstraints::new()
        .ca()
        .build()
        .context("failed to build BasicConstraints extension")?;
    x509builder
        .append_extension(bc)
        .context("failed to append BasicConstraints extension")?;

    // the akid depends on the skid. I guess it copies the skid when the cert is
    // self-signed or something, I'm not really sure.
    let skid = {
        // we need to wrap these in a block because the builder gets borrowed away
        // from us
        let ext_con = x509builder.x509v3_context(None, None);
        boring::x509::extension::SubjectKeyIdentifier::new()
            .build(&ext_con)
            .context("failed to build SubjectKeyIdentifier extention")?
    };
    x509builder
        .append_extension(skid)
        .context("failed to append SubjectKeyIdentifier extention")?;

    // now that the skid is added we can add the akid
    let akid = {
        let ext_con = x509builder.x509v3_context(None, None);
        boring::x509::extension::AuthorityKeyIdentifier::new()
            .keyid(true)
            .issuer(false)
            .build(&ext_con)
            .context("failed to build AuthorityKeyIdentifier extention")?
    };
    x509builder
        .append_extension(akid)
        .context("failed to append AuthorityKeyIdentifier extention")?;

    // self-sign the certificate
    x509builder
        .sign(&pkey, boring::hash::MessageDigest::sha256())
        .context("failed to self-sign x509 cert")?;

    let x509 = x509builder.build();

    Ok((x509, pkey))
}

fn big_endian_to_little_endian_padded(len: usize, num: boring::bn::BigNum) -> Result<Vec<u8>, anyhow::Error> {
    let mut out = vec![0u8; len];
    let bytes = swap_endianness(num.to_vec());
    let mut num_bytes = bytes.len();
    if len < num_bytes {
        if !fit_in_bytes(bytes.as_ref(), num_bytes, len) {
            return Err(anyhow!("Can't fit in bytes"));
        }
        num_bytes = len;
    }
    out[..num_bytes].copy_from_slice(&bytes[..num_bytes]);
    return Ok(out);
}

fn fit_in_bytes(bytes: &Vec<u8>, num_bytes: usize, len: usize) -> bool {
    let mut mask = 0u8;
    for i in len..num_bytes {
        mask |= bytes[i];
    }
    return mask == 0;
}

fn swap_endianness(bytes: Vec<u8>) -> Vec<u8> {
    bytes.into_iter().rev().collect()
}

pub fn encode_rsa_publickey(public_key: boring::rsa::Rsa<boring::pkey::Public>) -> Result<Vec<u8>, anyhow::Error> {
    let mut r32: boring::bn::BigNum;
    let mut n0inv: boring::bn::BigNum;
    let mut rr: boring::bn::BigNum;

    let mut tmp: boring::bn::BigNum;

    let mut ctx = boring::bn::BigNumContext::new()?;

    if (public_key.n().to_vec().len() as i32) < ANDROID_PUBKEY_MODULUS_SIZE {
        return Err(anyhow!(String::from("Invalid key length ") + public_key.n().to_vec().len().to_string().as_str()));
    }

    let mut key_struct = bytebuffer::ByteBuffer::new();
    key_struct.resize(ANDROID_PUBKEY_ENCODED_SIZE as usize);
    key_struct.set_endian(bytebuffer::Endian::LittleEndian);
    key_struct.write_i32(ANDROID_PUBKEY_MODULUS_SIZE_WORDS);

    // Compute and store n0inv = -1 / N[0] mod 2 ^ 32
    r32 = boring::bn::BigNum::new()?;
    r32.set_bit(32)?;
    n0inv = public_key.n().to_owned()?;
    tmp = n0inv.to_owned()?;
    // do n0inv mod r32
    n0inv.checked_rem(tmp.as_mut(), r32.as_ref(), ctx.as_mut())?;
    tmp = n0inv.to_owned()?;
    n0inv.mod_inverse(tmp.as_mut(), r32.as_ref(), ctx.as_mut())?;
    tmp = n0inv.to_owned()?;
    n0inv.checked_sub(r32.as_ref(), tmp.as_mut())?;

    // This is hacky.....
    key_struct.write_u32(n0inv.to_dec_str().unwrap().parse::<u32>().unwrap());

    key_struct.write(big_endian_to_little_endian_padded(
        ANDROID_PUBKEY_MODULUS_SIZE as usize,
        public_key.n().to_owned().unwrap())
        .unwrap().as_slice())?;

    rr = boring::bn::BigNum::new()?;
    rr.set_bit(ANDROID_PUBKEY_MODULUS_SIZE * 8)?;
    tmp = rr.to_owned()?;
    rr.mod_sqr(tmp.as_ref(), public_key.n().to_owned().unwrap().as_ref(), ctx.as_mut())?;

    key_struct.write(big_endian_to_little_endian_padded(
        ANDROID_PUBKEY_MODULUS_SIZE as usize,
        rr.to_owned().unwrap())
        .unwrap().as_slice())?;

    println!("{:?}", public_key.e().to_string().parse::<i32>().unwrap());
    key_struct.write_i32(public_key.e().to_string().parse::<i32>().unwrap());

    Ok(key_struct.into_vec())
}

fn encode_rsa_publickey_with_name(public_key: boring::rsa::Rsa<boring::pkey::Public>) -> Result<Vec<u8>, anyhow::Error> {
    let name = " Ascent@Antagonism\u{0}";
    let pkey_size = 4 * (f64::from(ANDROID_PUBKEY_ENCODED_SIZE) / 3.0).ceil() as usize;
    let mut bos = bytebuffer::ByteBuffer::new();
    bos.resize(pkey_size + name.len());
    let base64 = boring::base64::encode_block(encode_rsa_publickey(public_key).unwrap().as_slice());
    bos.write(base64.as_bytes())?;
    bos.write(name.as_bytes())?;
    Ok(bos.into_vec())
}

pub async fn pair(port: String, code: String, data_folder: String) -> Result<bool> {
    let host = "127.0.0.1".to_string();
    // let port = "45393".to_string();
    // let code = "312501".to_string();

    // Check cert file existance
    let data_folder = data_folder;

    let log = data_folder.clone() + "/log.txt";
    let log = std::path::Path::new(log.as_str());
    let mut log = std::fs::File::create(log).unwrap();

    let cert_path = data_folder.clone() + "/cert.pem";
    let pkey_path = data_folder.clone() + "/pkey.pem";
    let cert_path = std::path::Path::new(cert_path.as_str());
    let pkey_path = std::path::Path::new(pkey_path.as_str());

    let x509: Option<boring::x509::X509>;
    let pkey: Option<boring::pkey::PKey<boring::pkey::Private>>;

    if !cert_path.exists() || !pkey_path.exists() {
        let (x509_raw, pkey_raw) = generate_cert()?;
        x509 = Some(x509_raw.clone());
        pkey = Some(pkey_raw.clone());
        let mut cert_file = std::fs::File::create(cert_path)?;
        let mut pkey_file = std::fs::File::create(pkey_path)?;
        cert_file.write_all(x509_raw.to_pem().unwrap().as_slice())?;
        pkey_file.write_all(pkey_raw.private_key_to_pem_pkcs8().unwrap().as_slice())?;
    } else {
        return Ok(true);
    }

    // x509.unwrap().public_key().unwrap().rsa()

    let domain = host.clone() + ":" + port.as_str();
    let method = boring::ssl::SslMethod::tls();
    let mut connector = boring::ssl::SslConnector::builder(method)?;
    connector.set_verify(boring::ssl::SslVerifyMode::PEER);
    // The following two line is critical for ADB client auth, without them system_server will throw out "No peer certificate" error.
    connector.set_certificate(x509.clone().unwrap().as_ref())?;
    connector.set_private_key(pkey.clone().unwrap().as_ref())?;

    let mut config = connector.build().configure()?;
    config.set_verify_callback(boring::ssl::SslVerifyMode::PEER, |_, _| true);
    let stream = tokio::net::TcpStream::connect(domain.as_str()).await?;
    let mut stream = tokio_boring::connect(config, host.as_str(), stream).await?;
    // To ensure the connection is not stolen while we do the PAKE, append the exported key material from the
    // tls connection to the password.
    let mut exported_key_material = [0; 64];
    stream.ssl().export_keying_material(&mut exported_key_material, EXPORTED_KEY_LABEL, None)?;
    log.write_all(format!("exported_key_material: {:?}\n", exported_key_material).as_bytes())?;


    let mut password = vec![0u8; code.as_bytes().len() + exported_key_material.len()];
    password[..code.as_bytes().len()].copy_from_slice(code.as_bytes());
    password[code.as_bytes().len()..].copy_from_slice(&exported_key_material);
    let spake2_context = boring::curve25519::Spake2Context::new(
        boring::curve25519::Spake2Role::Alice,
        CLIENT_NAME,
        SERVER_NAME,
    )?;
    let mut outbound_msg = vec![0u8; 32];
    spake2_context.generate_message(outbound_msg.as_mut_slice(), 32, password.as_ref())?;

    // Set header
    let mut header = bytebuffer::ByteBuffer::new();
    header.resize(6);
    header.set_endian(bytebuffer::Endian::BigEndian);
    // Write in data
    // Write version
    header.write_u8(1);
    // Write message type
    header.write_u8(0);
    // Write message length
    header.write_i32(outbound_msg.len() as i32);

    // Send data
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(outbound_msg.as_slice()).await?;
    log.write_all(("SPAKE2 Send\n").as_bytes())?;

    // Read data
    stream.read_u8().await?;
    let msg_type = stream.read_u8().await?;
    let payload_length = stream.read_i32().await?;
    if msg_type != 0u8 {
        log.write_all(("Message type miss match\n").as_bytes())?;
        panic!("Message type miss match");
    }

    let mut payload_raw = vec![0u8; payload_length as usize];
    stream.read_exact(payload_raw.as_mut_slice()).await?;

    let mut bob_key = vec![0u8; 64];
    spake2_context.process_message(bob_key.as_mut_slice(), 64, payload_raw.as_mut_slice())?;

    // Has checked the hkdf generation process is correct
    // TODO: Check if the bob key is the same
    log.write_all(format!("bob key: {:?}\n", boring::base64::encode_block(bob_key.as_slice())).as_bytes())?;
    let mut secret_key = [0u8; 16];
    hkdf::Hkdf::<sha2::Sha256>::new(None, bob_key.as_ref()).expand("adb pairing_auth aes-128-gcm key".as_bytes(), &mut secret_key).unwrap();
    log.write_all(format!("secret key: {:?}\n", boring::base64::encode_block(secret_key.as_slice())).as_bytes())?;

    let encrypt_iv: i64 = 0;

    let mut iv_bytes = bytebuffer::ByteBuffer::new();
    iv_bytes.resize(12);
    iv_bytes.set_endian(bytebuffer::Endian::LittleEndian);
    iv_bytes.write_i64(encrypt_iv);

    let iv = iv_bytes.as_bytes();

    log.write_all(("Create Crypter\n").as_bytes())?;
    let mut crypter = boring::symm::Crypter::new(
        boring::symm::Cipher::aes_128_gcm(),
        boring::symm::Mode::Encrypt,
        secret_key.as_ref(),
        Some(iv))?;
    log.write_all(("Crypter Created\n").as_bytes())?;

    log.write_all(("Generate PeerInfo\n").as_bytes())?;
    let mut peerinfo = bytebuffer::ByteBuffer::new();
    peerinfo.resize(MAX_PEER_INFO_SIZE as usize);
    peerinfo.set_endian(bytebuffer::Endian::BigEndian);

    peerinfo.write_u8(0);
    peerinfo.write(encode_rsa_publickey_with_name(x509.unwrap().public_key().unwrap().rsa().unwrap()).unwrap().as_slice())?;
    log.write_all(("PeerInfo Generated\n").as_bytes())?;

    log.write_all(("Update Crypter\n").as_bytes())?;
    let mut encrypted = vec![0u8; peerinfo.as_bytes().len()];
    crypter.update(peerinfo.as_bytes(), encrypted.as_mut_slice())?;
    log.write_all(("Crypter Updated\n").as_bytes())?;
    let fin = crypter.finalize(encrypted.as_mut_slice())?;
    if fin != 0 {
        log.write_all(("Finalize error").as_bytes())?;
        panic!("Finalize error");
    }

    let mut encryption_tag = vec![0u8; 16];
    crypter.get_tag(encryption_tag.as_mut_slice())?;
    encrypted.append(encryption_tag.as_mut());
    // Set header    // Write version
    let mut header = bytebuffer::ByteBuffer::new();
    header.resize(6);
    header.set_endian(bytebuffer::Endian::BigEndian);
    // Write in data
    header.write_u8(1);
    // Write message type
    header.write_u8(1);
    // Write message length
    header.write_i32(encrypted.len() as i32);

    stream.write_all(header.as_bytes()).await?;
    stream.write_all(encrypted.as_slice()).await?;

    stream.flush().await?;
    log.write_all(("All process done").as_bytes())?;
    Ok(true)
}