#[error_code]
pub enum ErrorCode {
    #[msg("Amount cannot be zero.")]
    ZeroAmount,
    #[msg("Arithmetic overflow occurred.")]
    MathOverflow,
    #[msg("This asset is not supported by the protocol.")]
    UnsupportedAsset,
}