//! Generated typed application client bindings.

/// Generates scoped typed Cell accessors from explicit namespace and operation IDs.
///
/// Each generated constructor checks its declared IDs against the compiled
/// application and typed operation traits before any call can start.
///
/// ```no_run
/// mod proof {
///     # include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/generated_compile_setup.rs"));
///     # use crab_cell_runtime::MutationIdentity;
///     fn typed(cell: &Entity, identity: MutationIdentity) {
///         let _ = cell.set(identity, ());
///     }
/// }
/// ```
///
/// ```compile_fail
/// mod proof {
///     # include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/generated_compile_setup.rs"));
///     # use crab_cell_runtime::MutationIdentity;
///     fn wrong_input(cell: &Entity, identity: MutationIdentity) {
///         let _ = cell.set(identity, 42_u32);
///     }
/// }
/// ```
///
/// ```compile_fail
/// mod proof {
///     # include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/generated_compile_setup.rs"));
///     # use crab_cell_runtime::MutationIdentity;
///     fn unlisted_operation(cell: &Entity, identity: MutationIdentity) {
///         let _ = cell.delete(identity, ());
///     }
/// }
/// ```
///
/// ```compile_fail
/// mod proof {
///     # include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/generated_compile_setup.rs"));
///     fn wrong_key(client: &Client) {
///         let _ = client.entity(b"untyped-key");
///     }
/// }
/// ```
#[macro_export]
macro_rules! cell_client {
    (
        $visibility:vis struct $client:ident ($application:ty) {
            $(
                $cell_visibility:vis fn $accessor:ident ( $scope:ident : &$key:ty ) -> $cell:ident {
                    namespace: $namespace:expr,
                    module: $module:expr,
                    commands: { $( $command_visibility:vis fn $command_method:ident, $prepare_method:ident : $command:ty = $command_id:expr; )* },
                    queries: { $( $query_visibility:vis fn $query_method:ident : $query:ty = $query_id:expr; )* }
                }
            )*
        }
    ) => {
        $visibility struct $client {
            handle: $crate::ApplicationHandle<$application>,
        }

        impl $client {
            /// Validates generated bindings before accepting an application handle.
            $visibility fn new(handle: $crate::ApplicationHandle<$application>) -> crab_cell_runtime::Result<Self> {
                $(
                    let namespace: crab_cell_runtime::NamespaceId = const { $namespace };
                    let module: &'static str = const { $module };
                    let declared = handle.compiled().cell_types().iter()
                        .find(|cell_type| cell_type.namespace() == namespace)
                        .ok_or(crab_cell_runtime::Error::Registry("generated namespace is not declared"))?;
                    if declared.module() != module {
                        return Err(crab_cell_runtime::Error::Registry("generated module differs from descriptor"));
                    }
                    $(
                        if <$command as crab_cell_runtime::registry::Command>::MODULE != module
                            || <$command as crab_cell_runtime::registry::Command>::ID != const { $command_id }
                        {
                            return Err(crab_cell_runtime::Error::Registry("generated command differs from stable ID"));
                        }
                        handle.compiled().registry().command_contract::<$command>(namespace)?;
                    )*
                    $(
                        if <$query as crab_cell_runtime::registry::Query>::MODULE != module
                            || <$query as crab_cell_runtime::registry::Query>::ID != const { $query_id }
                        {
                            return Err(crab_cell_runtime::Error::Registry("generated query differs from stable ID"));
                        }
                        handle.compiled().registry().query_contract::<$query>(namespace)?;
                    )*
                )*
                Ok(Self { handle })
            }

            /// Resolves an ambiguous mutation through the application's durable request ledger.
            $visibility async fn resolve(
                &self,
                pending: &crab_cell_runtime::client::PendingMutation,
            ) -> std::result::Result<
                crab_cell_runtime::cell::executor::Resolution,
                crab_cell_runtime::client::InvocationError<Vec<u8>>,
            > {
                self.handle.resolve(pending).await
            }

            $(
                /// Selects a Cell using the descriptor's canonical shard function.
                $cell_visibility fn $accessor(&self, $scope: &$key) -> crab_cell_runtime::Result<$cell> {
                    let key = <$key as $crate::CellKey>::canonical_bytes($scope);
                    Ok($cell {
                        handle: self.handle.clone(),
                        target: self.handle.target_for_scope(const { $namespace }, key)?,
                    })
                }
            )*
        }

        $(
            $cell_visibility struct $cell {
                handle: $crate::ApplicationHandle<$application>,
                target: crab_cell_runtime::identity::CellTarget,
            }

            impl $cell {
                /// Returns the stable scoped target selected by this generated binding.
                $cell_visibility fn target(&self) -> &crab_cell_runtime::identity::CellTarget {
                    &self.target
                }

                $(
                    /// Invokes the declared typed command on this scoped Cell.
                    $command_visibility async fn $command_method(
                        &self,
                        identity: crab_cell_runtime::MutationIdentity,
                        input: <$command as crab_cell_runtime::registry::Command>::Input,
                    ) -> std::result::Result<
                        crab_cell_runtime::client::Committed<<$command as crab_cell_runtime::registry::Command>::Output>,
                        crab_cell_runtime::client::InvocationError<<$command as crab_cell_runtime::registry::Command>::Output>,
                    > {
                        self.handle.command::<$command>(&self.target, identity, input).await
                    }

                    /// Prepares the declared command for outcome-aware execution.
                    $command_visibility async fn $prepare_method(
                        &self,
                        identity: crab_cell_runtime::MutationIdentity,
                        input: <$command as crab_cell_runtime::registry::Command>::Input,
                    ) -> std::result::Result<
                        crab_cell_runtime::client::PreparedCommand<$command>,
                        crab_cell_runtime::client::InvocationError<<$command as crab_cell_runtime::registry::Command>::Output>,
                    > {
                        self.handle.prepare_command::<$command>(&self.target, identity, input).await
                    }
                )*

                $(
                    /// Invokes the declared typed query on this scoped Cell.
                    $query_visibility async fn $query_method(
                        &self,
                        minimum: Option<crab_cell_runtime::Receipt>,
                        input: <$query as crab_cell_runtime::registry::Query>::Input,
                    ) -> std::result::Result<
                        crab_cell_runtime::client::Observed<<$query as crab_cell_runtime::registry::Query>::Output>,
                        crab_cell_runtime::client::InvocationError<<$query as crab_cell_runtime::registry::Query>::Output>,
                    > {
                        self.handle.query::<$query>(&self.target, minimum, input).await
                    }
                )*
            }
        )*
    };
}
