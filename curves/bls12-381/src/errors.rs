use thiserror::Error;

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum BlsError {
    #[error("encoding failed")]
    BadEncoding,
    #[error("point is not on curve")]
    PointNotOnCurve,
    #[error("point is not in group")]
    PointNotInGroup,
    #[error("scalar failed")]
    BadScalar,
}
