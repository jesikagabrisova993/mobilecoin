// Copyright (c) 2018-2022 The MobileCoin Foundation

use displaydoc::Display;
use grpcio::RpcStatusCode;
use mc_attest_ake::Error as AkeError;
use mc_connection::AttestationError;
use mc_crypto_noise::CipherError;
use mc_util_serial::DecodeError;
use mc_util_uri::UriConversionError;

/// An error that can occur when using EnclaveConnection
#[derive(Display, Debug)]
pub enum Error {
    /// gRPC Error: {0}
    Rpc(grpcio::Error),
    /// Attestation AKE error: {0}
    Ake(AkeError),
    /// mc-crypto-noise cipher error: {0}
    Cipher(CipherError),
    /// Invalid Uri: {0}
    InvalidUri(UriConversionError),
    /// Protobuf deserialization: {0}
    ProtoDecode(DecodeError),
    /// Other: {0}
    Other(String),
}

impl AttestationError for Error {
    fn should_reattest(&self) -> bool {
        matches!(self, Self::Rpc(_) | Self::Ake(_) | Self::Cipher(_))
    }

    fn should_retry(&self) -> bool {
        match self {
            Error::Rpc(grpcio::Error::RpcFailure(rpc_status)) => {
                // Retry but only if the error code is not RESOURCE_EXHAUSTED, which is what is
                // returned when the response size is too large
                rpc_status.code() != RpcStatusCode::RESOURCE_EXHAUSTED
            }
            Error::Rpc(_) | Error::Cipher(_) | Error::ProtoDecode(_) => true,
            Error::Ake(AkeError::AttestationEvidenceVerification(_)) => false,
            Error::Ake(_) => true,
            Error::InvalidUri(_) => false,
            Error::Other(_) => false,
        }
    }
}

impl From<grpcio::Error> for Error {
    fn from(err: grpcio::Error) -> Self {
        Error::Rpc(err)
    }
}

impl From<AkeError> for Error {
    fn from(err: AkeError) -> Self {
        Error::Ake(err)
    }
}

impl From<CipherError> for Error {
    fn from(err: CipherError) -> Self {
        Error::Cipher(err)
    }
}

impl From<UriConversionError> for Error {
    fn from(src: UriConversionError) -> Self {
        Error::InvalidUri(src)
    }
}

impl From<DecodeError> for Error {
    fn from(src: DecodeError) -> Self {
        Error::ProtoDecode(src)
    }
}
