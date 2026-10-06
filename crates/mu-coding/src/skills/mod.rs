//! Skill activations that hand a session named authority. See `granted`.

pub mod granted;

pub use granted::{
    activate_granted_skill, build_activation, GrantedSkillActivation, GrantedSkillError,
};
