//! The reporting-line directory used to resolve `reporting_line` escalations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::eval::{Approver, ApproverType, EscalateTo};

/// One directory entry: a user's role and manager.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryUser {
    /// The user's role, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The user's manager id, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
}

/// The approver chain directory (`KERNOS_DATA/directory.json`):
/// `{"users": {"u-tom": {"role": "finance_admin", "manager": "u-cfo"}}}`. It may
/// be empty, in which case every reporting-line escalation lands on `role("admin")`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    /// Users by id. A BTreeMap keeps role lookups deterministic.
    #[serde(default)]
    pub users: BTreeMap<String, DirectoryUser>,
}

impl Directory {
    /// An empty directory.
    pub fn empty() -> Self {
        Directory::default()
    }

    /// The manager of a user, when the directory knows one.
    pub fn manager_of(&self, user: &str) -> Option<&str> {
        self.users.get(user).and_then(|u| u.manager.as_deref())
    }

    /// Resolves `reporting_line` for the current approver: the manager of a user
    /// approver when the directory knows one, else `role("admin")`. A role
    /// approver has no manager and escalates to `role("admin")`, as 04-POLICY says.
    pub fn reporting_line(&self, current: &Approver) -> Approver {
        let manager = match current.kind {
            ApproverType::User => self.manager_of(&current.value),
            ApproverType::Role => None,
        };
        match manager {
            Some(id) => Approver::user(id),
            None => Approver::role("admin"),
        }
    }

    /// Resolves an escalation target for the current approver.
    pub fn resolve_escalation(&self, target: &EscalateTo, current: &Approver) -> Approver {
        match target {
            EscalateTo::ReportingLine(_) => self.reporting_line(current),
            EscalateTo::Approver(approver) => approver.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> Directory {
        serde_json::from_value(serde_json::json!({
            "users": {
                "u-tom": {"role": "finance_admin", "manager": "u-cfo"},
                "u-ana": {"role": "ap_clerk", "manager": "u-tom"},
                "u-cfo": {"role": "cfo"}
            }
        }))
        .expect("directory")
    }

    #[test]
    fn user_escalates_to_manager() {
        let dir = directory();
        assert_eq!(
            dir.reporting_line(&Approver::user("u-tom")),
            Approver::user("u-cfo")
        );
        assert_eq!(
            dir.reporting_line(&Approver::user("u-cfo")),
            Approver::role("admin")
        );
        assert_eq!(
            dir.reporting_line(&Approver::user("nobody")),
            Approver::role("admin")
        );
    }

    #[test]
    fn role_approvers_escalate_to_admin() {
        let dir = directory();
        assert_eq!(
            dir.reporting_line(&Approver::role("finance_admin")),
            Approver::role("admin")
        );
        assert_eq!(
            Directory::empty().reporting_line(&Approver::user("u-tom")),
            Approver::role("admin")
        );
    }

    #[test]
    fn fixed_targets_pass_through() {
        let dir = directory();
        let target = EscalateTo::Approver(Approver::role("platform_owner"));
        assert_eq!(
            dir.resolve_escalation(&target, &Approver::user("u-tom")),
            Approver::role("platform_owner")
        );
    }
}
