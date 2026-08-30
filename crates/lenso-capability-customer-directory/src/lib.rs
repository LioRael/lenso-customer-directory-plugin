//! Generated portable Customer Directory Capability contract.

#![allow(clippy::struct_field_names, clippy::too_many_lines)]

include!("generated.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use lenso_kernel::{CancellationToken, InvocationContext, NativeRequestEndpoint};

    #[derive(Debug)]
    struct ContractFixture;

    impl CustomerDirectoryProvider for ContractFixture {
        fn get_contact(
            &self,
            _context: InvocationContext,
            request: GetContactRequest,
        ) -> lenso_kernel::NativeRequestFuture<CustomerDirectoryGetContact> {
            let contact_id = request.contact_id;
            let organization_id = request.organization_id;
            Box::pin(async move {
                Ok(Ok(GetContactResponse {
                    contact: Contact {
                        canonical_contact_id: contact_id.clone(),
                        contact_id,
                        created_at: "2026-08-31T00:00:00Z".to_owned(),
                        display_name: Some("Customer".to_owned()),
                        email: "customer@example.com".to_owned(),
                        organization_id,
                        revision: "1".to_owned(),
                        state: ContactState::Active,
                        updated_at: "2026-08-31T00:00:00Z".to_owned(),
                    },
                }))
            })
        }

        fn merge_contact(
            &self,
            _context: InvocationContext,
            _request: MergeContactRequest,
        ) -> lenso_kernel::NativeRequestFuture<CustomerDirectoryMergeContact> {
            Box::pin(async { Ok(Err(MergeContactError::RevisionConflict)) })
        }

        fn resolve_or_create_email_contact(
            &self,
            _context: InvocationContext,
            _request: ResolveOrCreateEmailContactRequest,
        ) -> lenso_kernel::NativeRequestFuture<CustomerDirectoryResolveOrCreateEmailContact>
        {
            Box::pin(async {
                Err(RuntimeFailure::PluginFailure {
                    detail: "fixture unavailable".to_owned(),
                })
            })
        }
    }

    fn context() -> InvocationContext {
        InvocationContext::new(1, None, CancellationToken::new())
    }

    #[test]
    fn generated_endpoint_preserves_success_domain_and_runtime_channels() {
        let endpoint = CustomerDirectoryEndpoint::new(ContractFixture);
        let contact_id = "00000000-0000-0000-0000-000000000001".to_owned();
        let success = futures::executor::block_on(endpoint.invoke(
            GET_CONTACT_OPERATION,
            Box::new(GetContactRequest {
                contact_id: contact_id.clone(),
                organization_id: "org_1".to_owned(),
            }),
            context(),
        ));
        let Ok(Ok(success)) = success else {
            panic!("success did not use the success channel");
        };
        let Ok(success) = success.downcast::<GetContactResponse>() else {
            panic!("success had the wrong response type");
        };
        assert_eq!(success.contact.contact_id, contact_id);

        let domain = futures::executor::block_on(endpoint.invoke(
            MERGE_CONTACT_OPERATION,
            Box::new(MergeContactRequest {
                expected_source_revision: "1".to_owned(),
                expected_target_revision: "1".to_owned(),
                idempotency_key: "merge_1".to_owned(),
                organization_id: "org_1".to_owned(),
                source_contact_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                target_contact_id: "00000000-0000-0000-0000-000000000002".to_owned(),
            }),
            context(),
        ));
        let Ok(Err(domain)) = domain else {
            panic!("domain rejection did not use the domain channel");
        };
        let Ok(domain) = domain.downcast::<MergeContactError>() else {
            panic!("domain result had the wrong error type");
        };
        assert_eq!(*domain, MergeContactError::RevisionConflict);

        let runtime = futures::executor::block_on(endpoint.invoke(
            RESOLVE_OR_CREATE_EMAIL_CONTACT_OPERATION,
            Box::new(ResolveOrCreateEmailContactRequest {
                display_name: None,
                email: "customer@example.com".to_owned(),
                idempotency_key: "message_1".to_owned(),
                organization_id: "org_1".to_owned(),
            }),
            context(),
        ));
        let Err(runtime) = runtime else {
            panic!("runtime failure did not use the runtime channel");
        };
        assert_eq!(
            runtime,
            RuntimeFailure::PluginFailure {
                detail: "fixture unavailable".to_owned()
            }
        );
    }
}
