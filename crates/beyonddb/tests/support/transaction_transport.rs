// Exercise both scoped ApplicationHandle and CellClient callers through the
// same registered upload and phase commands used by the production adapter.
#[macro_export]
macro_rules! transaction_command {
    ($client:expr, $command:ty, $target:expr, $identity:expr, $input:expr $(,)?) => {
        async {
            let client = &$client;
            let target = $target;
            let identity = $identity;
            let input = $input;
            let bytes = serde_json::to_vec(&input.0).unwrap();
            let reference =
                beyonddb::TransactionPayloadRef::new(&bytes, identity.expires_at_ms).unwrap();
            for chunk in reference.chunks(&bytes) {
                let identity = crab_cell_runtime::MutationIdentity {
                    request_id: crab_cell_runtime::identity::RequestId::from_bytes(
                        *uuid::Uuid::now_v7().as_bytes(),
                    ),
                    ..identity
                };
                client
                    .command::<beyonddb::UploadTransactionPayload<$command>>(
                        target, identity, chunk,
                    )
                    .await
                    .unwrap();
            }
            client
                .command::<$command>(target, identity, beyonddb::Json(reference))
                .await
        }
    };
}
