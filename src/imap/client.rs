use std::{ops::{Deref, DerefMut}, time::Duration};

use async_imap::{
    error::{Error as ImapError, Result as ImapResult},
    Client as ImapClient,
};

use async_smtp::smtp::Socks5Config;
use async_smtp::smtp::ServerAddress;


use async_std::{future, net::{self, TcpStream}};
use fast_socks5::client::{Config, Socks5Stream};

use super::session::Session;
use crate::login_param::{DeltaSocks5Config, DeltaSocksError, dc_build_tls};

use super::session::SessionStream;


#[derive(Debug)]
pub(crate) struct Client {
    is_secure: bool,
    inner: ImapClient<Box<dyn SessionStream>>,
}

impl Deref for Client {
    type Target = ImapClient<Box<dyn SessionStream>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Client {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Client {
    pub async fn login(
        self,
        username: &str,
        password: &str,
    ) -> std::result::Result<Session, (ImapError, Self)> {
        let Client { inner, is_secure } = self;
        let session = inner
            .login(username, password)
            .await
            .map_err(|(err, client)| {
                (
                    err,
                    Client {
                        is_secure,
                        inner: client,
                    },
                )
            })?;
        Ok(Session { inner: session })
    }

    pub async fn authenticate(
        self,
        auth_type: &str,
        authenticator: impl async_imap::Authenticator,
    ) -> std::result::Result<Session, (ImapError, Self)> {
        let Client { inner, is_secure } = self;
        let session =
            inner
                .authenticate(auth_type, authenticator)
                .await
                .map_err(|(err, client)| {
                    (
                        err,
                        Client {
                            is_secure,
                            inner: client,
                        },
                    )
                })?;
        Ok(Session { inner: session })
    }

    pub async fn connect_secure(
        addr: impl net::ToSocketAddrs,
        domain: &str,
        strict_tls: bool,
    ) -> ImapResult<Self> {
        let stream = TcpStream::connect(addr).await?;
        let tls = dc_build_tls(strict_tls);
        let tls_stream: Box<dyn SessionStream> = Box::new(tls.connect(domain, stream).await?);
        let mut client = ImapClient::new(tls_stream);

        let _greeting = client
            .read_response()
            .await
            .ok_or_else(|| ImapError::Bad("failed to read greeting".to_string()))?;

        Ok(Client {
            is_secure: true,
            inner: client,
        })
    }

    pub async fn connect_insecure(addr: impl net::ToSocketAddrs) -> ImapResult<Self> {
        let stream: Box<dyn SessionStream> = Box::new(TcpStream::connect(addr).await?);

        let mut client = ImapClient::new(stream);
        let _greeting = client
            .read_response()
            .await
            .ok_or_else(|| ImapError::Bad("failed to read greeting".to_string()))?;

        Ok(Client {
            is_secure: false,
            inner: client,
        })
    }

    pub async fn connect_secure_socks5(target_addr: (String, u16), strict_tls: bool, socks5_config: DeltaSocks5Config) -> ImapResult<Self> {
        let socks5_stream: Box<dyn SessionStream> = Box::new(match socks5_config.connect(&ServerAddress::new(target_addr.0.clone(), target_addr.1), Duration::from_millis(500)).await {
            Ok(s) => s,
            Err(e) => { return Err(ImapError::ConnectionLost); }
        });

        let tls = dc_build_tls(strict_tls);
        let tls_stream: Box<dyn SessionStream> = Box::new(tls.connect(target_addr.0, socks5_stream).await?);
        let mut client = ImapClient::new(tls_stream);

        let _greeting = client
            .read_response()
            .await
            .ok_or_else(|| ImapError::Bad("failed to read greeting".to_string()))?;

        Ok(Client {
            is_secure: true,
            inner: client,
        })
    }

    pub async fn connect_insecure_socks5(target_addr: (String, u16), socks5_config: DeltaSocks5Config) -> ImapResult<Self> {
        println!("xxxx?");
        let socks5_stream: Box<dyn SessionStream> = Box::new(match socks5_config.connect(&ServerAddress::new(target_addr.0.clone(), target_addr.1), Duration::from_millis(5000)).await {
            Ok(s) => s,
            Err(e) => {
                return match e {
                    DeltaSocksError::SocksError(e) => Err(ImapError::Socks5Error(e)),
                    DeltaSocksError::TimeoutError(e) => Err(ImapError::Timeout(e))
                };
            }
        });


        let mut client = ImapClient::new(socks5_stream);
        let _greeting = client
            .read_response()
            .await
            .ok_or_else(|| ImapError::Bad("failed to read greeting".to_string()))?;

        Ok(Client {
            is_secure: false,
            inner: client,
        })
    }

    pub async fn secure(self, domain: &str, strict_tls: bool) -> ImapResult<Client> {
        if self.is_secure {
            Ok(self)
        } else {
            let Client { mut inner, .. } = self;
            let tls = dc_build_tls(strict_tls);
            inner.run_command_and_check_ok("STARTTLS", None).await?;

            let stream = inner.into_inner();
            let ssl_stream = tls.connect(domain, stream).await?;
            let boxed: Box<dyn SessionStream> = Box::new(ssl_stream);

            Ok(Client {
                is_secure: true,
                inner: ImapClient::new(boxed),
            })
        }
    }
}
