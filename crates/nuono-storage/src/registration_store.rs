use crate::{Database, entity::users, error::Result};
use nuono_app::account_registration::{
    RegistrationAccount, RegistrationReceipt, RegistrationStore,
};
impl From<users::Model> for RegistrationAccount {
    fn from(user: users::Model) -> Self {
        Self {
            id: user.id,
            username: user.username,
            email: user.email,
            status: user.status,
            email_verified: user.email_verified,
        }
    }
}
#[async_trait::async_trait]
impl RegistrationStore for Database {
    async fn registration_by_name(&self, username: &str) -> Result<Option<RegistrationAccount>> {
        Ok(self.find_user_by_username(username).await?.map(Into::into))
    }
    async fn registration_by_email(&self, email: &str) -> Result<Option<RegistrationAccount>> {
        Ok(self.find_user_by_email(email).await?.map(Into::into))
    }
    async fn create_registration_request(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<RegistrationReceipt> {
        self.prepare_registration(username, email, password).await
    }
    async fn registration_request_for_email(&self, email: &str) -> Result<Option<String>> {
        self.pending_registration_id(email).await
    }
}
