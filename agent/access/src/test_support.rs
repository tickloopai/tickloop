/// Small transport limits that let tests cross the frame and chunk boundaries with little data.
pub const TRANSPORT_LIMITS: crate::AccessLimits = match crate::AccessLimits::new(64 << 10, 16 << 10)
{
    Ok(limits) => limits,
    Err(_) => panic!("the test AccessLimits are valid"),
};
