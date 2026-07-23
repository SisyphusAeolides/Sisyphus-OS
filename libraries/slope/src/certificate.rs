use aether::certificate_page::CertificatePage;
use aether::transition_certificate::{CertificateError, CertificateOutcome, TransitionCertificate};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateReaderError {
    NullAddress,
    MisalignedAddress,
    IncompatiblePage,
    InvalidCertificate(CertificateError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EchoState {
    pub chain_root: u64,
    pub sequence: u64,
    pub verdict: u64,
}

pub struct CertificateReader {
    page: &'static CertificatePage,
}

impl CertificateReader {
    /// # Safety
    ///
    /// `address` must reference the process's read-only certificate page and
    /// remain mapped for the process lifetime.
    pub unsafe fn from_address(address: usize) -> Result<Self, CertificateReaderError> {
        if address == 0 {
            return Err(CertificateReaderError::NullAddress);
        }

        if address % core::mem::align_of::<CertificatePage>() != 0 {
            return Err(CertificateReaderError::MisalignedAddress);
        }

        // SAFETY: Established by the caller's mapping contract.
        let page = unsafe { &*(address as *const CertificatePage) };

        if !page.compatible() {
            return Err(CertificateReaderError::IncompatiblePage);
        }

        Ok(Self { page })
    }

    pub fn latest(&self) -> Result<Option<TransitionCertificate>, CertificateReaderError> {
        let Some(certificate) = self.page.snapshot() else {
            return Ok(None);
        };

        certificate
            .validate()
            .map_err(CertificateReaderError::InvalidCertificate)?;

        Ok(Some(certificate))
    }

    pub fn latest_committed(
        &self,
    ) -> Result<Option<TransitionCertificate>, CertificateReaderError> {
        let Some(certificate) = self.latest()? else {
            return Ok(None);
        };

        let outcome = CertificateOutcome::try_from(certificate.outcome)
            .map_err(CertificateReaderError::InvalidCertificate)?;

        Ok((outcome == CertificateOutcome::Committed).then_some(certificate))
    }

    pub fn echo_state(&self) -> EchoState {
        let (chain_root, sequence, verdict) = self.page.echo_state();

        EchoState {
            chain_root,
            sequence,
            verdict,
        }
    }
}
