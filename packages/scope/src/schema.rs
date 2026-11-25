//! What scopes exist: a namespace, its resources and each one's actions,
//! declared with [`schema!`](crate::schema!).

use crate::{Scope, Scopes, Segment};

/// One resource of a [`Schema`]: its path below the namespace and its
/// actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    /// `["wallets"]`, `["wallets", "legs"]`.
    pub path: &'static [&'static str],
    /// `["read", "write"]`.
    pub actions: &'static [&'static str],
}

/// Every scope there is: namespace `com.hashinbox`, its resources and
/// their actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schema {
    namespace: &'static [&'static str],
    resources: &'static [Resource],
}

impl Schema {
    /// `resources` under `namespace` (`["com", "hashinbox"]`).
    #[must_use]
    pub const fn new(namespace: &'static [&'static str], resources: &'static [Resource]) -> Self {
        Self {
            namespace,
            resources,
        }
    }

    /// Its resources.
    #[must_use]
    pub const fn resources(&self) -> &'static [Resource] {
        self.resources
    }

    /// Resource `resource`'s full path, every id.
    fn path_of(&self, resource: &Resource) -> Vec<Segment> {
        let names = self.namespace.iter().chain(resource.path);
        names
            .map(|name| Segment::Name {
                name: (*name).to_owned(),
                ids: None,
            })
            .collect()
    }

    /// Every concrete scope: each resource with each of its actions.
    #[must_use]
    pub fn scopes(&self) -> Scopes {
        let each = self.resources.iter().flat_map(|resource| {
            let path = self.path_of(resource);
            let actions = resource.actions.iter();
            actions.map(move |action| Scope::__from_parts(path.clone(), action))
        });
        each.collect()
    }

    /// The concrete scopes `scope` allows (no `*`), each keeping `scope`'s
    /// ids: `com.hashinbox.*.$read` is every resource's `$read`.
    #[must_use]
    pub fn expand(&self, scope: &Scope) -> Scopes {
        let every = without_ids(scope);
        let expanded = self
            .scopes()
            .into_iter()
            .filter(|concrete| every.covers(concrete));
        expanded
            .map(|concrete| with_ids(&concrete, scope))
            .collect()
    }

    /// Whether `scope` names something: it allows one of its scopes at
    /// least. A typo (`wallet.$read`, `wallets.$raed`) does not.
    #[must_use]
    pub fn knows(&self, scope: &Scope) -> bool {
        !self.expand(scope).is_empty()
    }
}

/// `scope` with every step's ids dropped.
fn without_ids(scope: &Scope) -> Scope {
    let path = scope.path().iter().map(|step| match step {
        Segment::Name { name, .. } => Segment::Name {
            name: name.clone(),
            ids: None,
        },
        Segment::Any => Segment::Any,
    });
    Scope::__from_parts(path.collect(), "*").with_action(scope.action().clone())
}

/// `concrete` with `scope`'s ids at the steps `scope` names.
fn with_ids(concrete: &Scope, scope: &Scope) -> Scope {
    let path =
        concrete
            .path()
            .iter()
            .enumerate()
            .map(|(at, step)| match (step, scope.path().get(at)) {
                (Segment::Name { name, .. }, Some(Segment::Name { ids: Some(ids), .. })) => {
                    Segment::Name {
                        name: name.clone(),
                        ids: Some(ids.clone()),
                    }
                }
                _ => step.clone(),
            });
    Scope::__from_parts(path.collect(), "*").with_action(concrete.action().clone())
}

#[cfg(test)]
mod tests {
    use crate::Scope;

    crate::schema! {
        /// The test schema.
        static TEST = com.hashinbox {
            wallets => [read, write],
            wallets.legs => [read],
            markets => [read],
        }
    }

    fn scope(text: &str) -> Scope {
        text.parse().unwrap()
    }

    #[test]
    fn expand_names_every_concrete_scope_keeping_ids() {
        let read = TEST.expand(&scope("com.hashinbox.*.$read"));
        assert_eq!(
            read.to_string(),
            "com.hashinbox.wallets.$read com.hashinbox.wallets.legs.$read \
             com.hashinbox.markets.$read"
        );
        let one = TEST.expand(&scope("com.hashinbox.wallets[x].*.$*"));
        assert_eq!(
            one.to_string(),
            "com.hashinbox.wallets[x].$read com.hashinbox.wallets[x].$write \
             com.hashinbox.wallets[x].legs.$read"
        );
    }

    #[test]
    fn knows_refuses_typos() {
        assert!(TEST.knows(&scope("com.hashinbox.wallets[x].$read")));
        assert!(!TEST.knows(&scope("com.hashinbox.wallet.$read")));
        assert!(!TEST.knows(&scope("com.hashinbox.wallets.$raed")));
        assert!(!TEST.knows(&scope("com.hashinbox.markets.$write")));
    }
}
