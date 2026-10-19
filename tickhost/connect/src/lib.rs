//! Host-side access to Ticks: per-instance Tick Sessions, the hosted launch contract, Herdr as the
//! layout host of Tick views, and where an Agent open finds a Tick's program.

pub mod executable_lease;
pub mod herdr;
pub mod host_error;
pub mod hosted;
pub mod launch;
pub mod layout;
pub mod naming;
pub mod page;
pub mod program;
pub mod session;
