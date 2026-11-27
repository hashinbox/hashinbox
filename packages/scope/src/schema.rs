//! What scopes exist: a namespace and its resources, a tree, declared
//! with [`schema!`](crate::schema!).

use crate::Scope;

/// Every scope there is: namespace `com.hashinbox` and the paths below it
/// (`["wallets"]`, `["wallets", "tags"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schema {
    namespace: &'static [&'static str],
    resources: &'static [&'static [&'static str]],
}

impl Schema {
    /// `resources` under `namespace` (`["com", "hashinbox"]`).
    #[must_use]
    pub const fn new(
        namespace: &'static [&'static str],
        resources: &'static [&'static [&'static str]],
    ) -> Self {
        Self {
            namespace,
            resources,
        }
    }

    /// Its resources' paths below the namespace.
    #[must_use]
    pub const fn resources(&self) -> &'static [&'static [&'static str]] {
        self.resources
    }

    /// Whether `scope` names something: the namespace itself (no ids), or
    /// one of its resources (ids on the resource's steps). A typo
    /// (`wallet`) does not.
    #[must_use]
    pub fn knows(&self, scope: &Scope) -> bool {
        let (namespace, rest) = scope
            .path()
            .split_at(self.namespace.len().min(scope.path().len()));
        let named = |steps: &[crate::Segment], names: &[&str]| {
            steps.len() == names.len()
                && steps
                    .iter()
                    .zip(names)
                    .all(|(step, name)| step.name == *name)
        };
        if !named(namespace, self.namespace) || namespace.iter().any(|step| step.id.is_some()) {
            return false;
        }
        rest.is_empty() || self.resources.iter().any(|resource| named(rest, resource))
    }
}

#[cfg(test)]
mod tests {
    use crate::Scope;

    crate::schema! {
        /// The test schema.
        static TEST = com.hashinbox {
            wallets,
            wallets.tags,
            markets,
        }
    }

    fn scope(text: &str) -> Scope {
        text.parse().unwrap()
    }

    #[test]
    fn knows_refuses_typos() {
        assert!(TEST.knows(&scope("com.hashinbox")));
        assert!(TEST.knows(&scope("com.hashinbox:read")));
        assert!(TEST.knows(&scope("com.hashinbox.wallets[x]:read")));
        assert!(TEST.knows(&scope("com.hashinbox.wallets[x].tags")));
        assert!(!TEST.knows(&scope("com")));
        assert!(!TEST.knows(&scope("com.hashinbox[x]")));
        assert!(!TEST.knows(&scope("com.hashinbox.wallet:read")));
        assert!(!TEST.knows(&scope("com.hashinbox.markets.tags")));
    }
}
