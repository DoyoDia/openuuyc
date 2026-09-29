//! Optional privileged host service; independent from either driver package.
pub(crate) mod install;
pub(crate) mod pipe;
pub(crate) mod process;
mod sas_policy;
pub(crate) mod service;

pub(crate) mod displays;
pub(crate) mod resident;
pub(crate) mod startup;
pub(crate) mod vault;
