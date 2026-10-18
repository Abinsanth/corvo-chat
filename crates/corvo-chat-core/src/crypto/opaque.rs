//! The client's OPAQUE steps, over the proto's cipher suite.

use argon2::Argon2;
use corvo_chat_proto::B64;
use corvo_chat_proto::opaque::{
    CredFinal, CredRequest, CredResponse, RegRequest, RegResponse, RegUpload, Suite, wrap_key,
};
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration, ClientRegistrationFinishParameters,
    CredentialResponse, Identifiers, RegistrationResponse,
};
use rand_core::OsRng;
use zeroize::Zeroizing;

use crate::{Error, Strength};

/// A registration waiting for the server's response.
pub(crate) struct Registering(ClientRegistration<Suite>);

/// A login waiting for the server's response.
pub(crate) struct LoggingIn(ClientLogin<Suite>);

fn b64<const N: usize>(bytes: impl AsRef<[u8]>) -> B64<N> {
    B64(bytes.as_ref().try_into().expect("OPAQUE messages have fixed sizes"))
}

/// The key stretching for the password. The recovery code always uses
/// `recovery_ksf()` instead.
pub(crate) fn password_ksf(s: Strength) -> Argon2<'static> {
    match s {
        Strength::Normal => corvo_chat_proto::opaque::password_ksf(),
        #[cfg(feature = "insecure-test-kdf")]
        Strength::FastInsecure => corvo_chat_proto::opaque::recovery_ksf(),
    }
}

pub(crate) fn register_start(secret: &[u8]) -> (Registering, RegRequest) {
    let start = ClientRegistration::<Suite>::start(&mut OsRng, secret).expect("a registration request");
    (Registering(start.state), b64(start.message.serialize()))
}

/// Finishes a registration: the upload for the server, and the wrap key.
pub(crate) fn register_finish(
    r: Registering,
    secret: &[u8],
    response: &RegResponse,
    ksf: &Argon2<'static>,
) -> Result<(RegUpload, Zeroizing<[u8; 32]>), Error> {
    let response = RegistrationResponse::<Suite>::deserialize(&response.0).map_err(|_| Error::BadServerMessage)?;
    let params = ClientRegistrationFinishParameters::new(Identifiers::default(), Some(ksf));
    let finish = r.0.finish(&mut OsRng, secret, response, params).map_err(|_| Error::BadServerMessage)?;
    Ok((b64(finish.message.serialize()), Zeroizing::new(wrap_key(&finish.export_key))))
}

pub(crate) fn login_start(secret: &[u8]) -> (LoggingIn, CredRequest) {
    let start = ClientLogin::<Suite>::start(&mut OsRng, secret).expect("a credential request");
    (LoggingIn(start.state), b64(start.message.serialize()))
}

/// Finishes a login: the proof for the server, and the wrap key. A wrong
/// secret (or an unknown name, which the server answers with a fake record)
/// is `WrongCredentials`.
pub(crate) fn login_finish(
    l: LoggingIn,
    secret: &[u8],
    response: &CredResponse,
    ksf: &Argon2<'static>,
) -> Result<(CredFinal, Zeroizing<[u8; 32]>), Error> {
    let response = CredentialResponse::<Suite>::deserialize(&response.0).map_err(|_| Error::WrongCredentials)?;
    let params = ClientLoginFinishParameters::new(None, Identifiers::default(), Some(ksf));
    let finish = l.0.finish(&mut OsRng, secret, response, params).map_err(|_| Error::WrongCredentials)?;
    Ok((b64(finish.message.serialize()), Zeroizing::new(wrap_key(&finish.export_key))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Strength};
    use corvo_chat_proto::opaque::Suite;
    use opaque_ke::{
        CredentialFinalization, CredentialRequest, RegistrationRequest, RegistrationUpload, ServerLogin,
        ServerLoginParameters, ServerRegistration, ServerSetup,
    };
    use rand_core::OsRng;

    const ID: &[u8] = b"password:alice";

    fn log_in(
        setup: &ServerSetup<Suite>,
        record: &ServerRegistration<Suite>,
        secret: &[u8],
    ) -> Result<(bool, Zeroizing<[u8; 32]>), Error> {
        let (state, request) = login_start(secret);
        let request = CredentialRequest::<Suite>::deserialize(&request.0).unwrap();
        let started =
            ServerLogin::start(&mut OsRng, setup, Some(record.clone()), request, ID, ServerLoginParameters::default())
                .unwrap();
        let response = B64(started.message.serialize().as_slice().try_into().unwrap());
        let ksf = password_ksf(Strength::FastInsecure);
        let (fin, key) = login_finish(state, secret, &response, &ksf)?;
        let fin = CredentialFinalization::<Suite>::deserialize(&fin.0).unwrap();
        Ok((started.state.finish(fin, ServerLoginParameters::default()).is_ok(), key))
    }

    #[test]
    fn opaque_steps_round_trip_against_a_server() {
        let setup = ServerSetup::<Suite>::new(&mut OsRng);
        let ksf = password_ksf(Strength::FastInsecure);

        let (state, request) = register_start(b"pw");
        let request = RegistrationRequest::<Suite>::deserialize(&request.0).unwrap();
        let started = ServerRegistration::<Suite>::start(&setup, request, ID).unwrap();
        let response = B64(started.message.serialize().as_slice().try_into().unwrap());
        let (upload, registered_key) = register_finish(state, b"pw", &response, &ksf).unwrap();
        let upload = RegistrationUpload::<Suite>::deserialize(&upload.0).unwrap();
        let record = ServerRegistration::<Suite>::finish(upload);

        let (proven, key) = log_in(&setup, &record, b"pw").unwrap();
        assert!(proven);
        assert_eq!(*key, *registered_key);
        assert!(matches!(log_in(&setup, &record, b"wrong"), Err(Error::WrongCredentials)));
    }
}
