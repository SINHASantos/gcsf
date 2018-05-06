use drive3;
use failure::{err_msg, Error, ResultExt};
use hyper;
use hyper_rustls;
use oauth2;
use serde_json;
use std::io::{Read, Seek, SeekFrom};
use std::io;
use std::cmp;
use std::collections::HashMap;
use super::DataFetcher;

type Inode = u64;

type GCClient = hyper::Client;
type GCAuthenticator = oauth2::Authenticator<
    oauth2::DefaultAuthenticatorDelegate,
    oauth2::DiskTokenStorage,
    hyper::Client,
>;
type GCDrive = drive3::Drive<GCClient, GCAuthenticator>;

pub struct GoogleDriveFetcher {
    hub: GCDrive,
    buff: Vec<u8>,
    pending_writes: HashMap<Inode, Vec<PendingWrite>>,
}

struct PendingWrite {
    inode: Inode,
    offset: usize,
    data: Vec<u8>,
}

impl GoogleDriveFetcher {
    fn read_client_secret(file: &str) -> Result<oauth2::ApplicationSecret, Error> {
        use std::fs::OpenOptions;
        use std::io::Read;

        let mut file = OpenOptions::new().read(true).open(file)?;

        let mut secret = String::new();
        file.read_to_string(&mut secret);

        let app_secret: oauth2::ConsoleApplicationSecret = serde_json::from_str(secret.as_str())?;
        app_secret
            .installed
            .ok_or(err_msg("Option did not contain a value."))
    }

    fn create_drive_auth() -> Result<GCAuthenticator, Error> {
        // Get an ApplicationSecret instance by some means. It contains the `client_id` and
        // `client_secret`, among other things.
        //
        let secret: oauth2::ApplicationSecret = Self::read_client_secret("client_secret.json")?;

        // Instantiate the authenticator. It will choose a suitable authentication flow for you,
        // unless you replace  `None` with the desired Flow.
        // Provide your own `AuthenticatorDelegate` to adjust the way it operates
        // and get feedback about
        // what's going on. You probably want to bring in your own `TokenStorage`
        // to persist tokens and
        // retrieve them from storage.
        let auth = oauth2::Authenticator::new(
            &secret,
            oauth2::DefaultAuthenticatorDelegate,
            hyper::Client::with_connector(hyper::net::HttpsConnector::new(
                hyper_rustls::TlsClient::new(),
            )),
            // <MemoryStorage as Default>::default(),
            oauth2::DiskTokenStorage::new(&String::from("/tmp/gcsf_token.json")).unwrap(),
            Some(oauth2::FlowType::InstalledRedirect(8080)), // This is the main change!
        );

        Ok(auth)
    }

    fn create_drive() -> Result<GCDrive, Error> {
        let auth = Self::create_drive_auth()?;
        Ok(drive3::Drive::new(
            hyper::Client::with_connector(hyper::net::HttpsConnector::new(
                hyper_rustls::TlsClient::new(),
            )),
            auth,
        ))
    }

    // Will still detect a file even if it is in Trash
    fn file_exists(&self, name: &str) -> drive3::Result<()> {
        self.get_file_id(name).and_then(|id| Ok(()))
    }

    fn get_file_id(&self, name: &str) -> drive3::Result<String> {
        let query = format!("name=\"{}\"", name);
        let (search_response, file_list) = self.hub
            .files()
            .list()
            .spaces("drive")
            .corpora("user")
            .q(&query)
            .page_size(1)
            .add_scope(drive3::Scope::Full)
            .doit()?;

        let metadata = file_list
            .files
            .map(|files| files.into_iter().take(1).next())
            .ok_or(drive3::Error::FieldClash("haha"))?
            .ok_or(drive3::Error::Failure(search_response))?;

        metadata.id.ok_or(drive3::Error::FieldClash(
            "file metadata does not contain an id",
        ))
    }

    fn get_file_content(&self, name: &str) -> drive3::Result<Vec<u8>> {
        let file_id = self.get_file_id(name)?;

        let (mut response, empty_file) = self.hub
            .files()
            .get(&file_id)
            .supports_team_drives(false)
            .param("alt", "media")
            .add_scope(drive3::Scope::Full)
            .doit()?;

        let mut content: Vec<u8> = Vec::new();
        response.read_to_end(&mut content);

        Ok(content)
    }

    fn apply_pending_writes_on_data(&mut self, inode: Inode, data: &mut Vec<u8>) {
        self.pending_writes
            .entry(inode)
            .or_insert(Vec::new())
            .iter()
            .filter(|write| write.inode == inode)
            .for_each(|pending_write| {
                info!("found a pending write! applying now");
                let required_size = pending_write.offset + pending_write.data.len();

                data.resize(required_size, 0);

                // TODO: memcpy or similar
                for i in pending_write.offset..pending_write.offset + pending_write.data.len() {
                    data[i] = pending_write.data[i - pending_write.offset];
                }
            });

        self.pending_writes.remove(&inode);
    }
    // fn ls(&self) -> Vec<drive3::File> {
    //     let result = self.drive.files()
    //     .list()
    //     .spaces("drive")
    //     .page_size(10)
    //     // .order_by("folder,modifiedTime desc,name")
    //     .corpora("user") // or "domain"
    //     .doit();

    //     match result {
    //         Err(e) => {
    //             println!("{:#?}", e);
    //             vec![]
    //         }
    //         Ok(res) => res.1.files.unwrap().into_iter().collect(),
    //     }
    // }

    // fn cat(&self, filename: &str) -> String {
    //     let result = self.drive.files()
    //     .list()
    //     .spaces("drive")
    //     .page_size(10)
    //     // .order_by("folder,modifiedTime desc,name")
    //     .corpora("user") // or "domain"
    //     .doit();
    // }
}

impl DataFetcher for GoogleDriveFetcher {
    fn new() -> GoogleDriveFetcher {
        debug!("GoogleDriveFetcher::new()");
        GoogleDriveFetcher {
            hub: GoogleDriveFetcher::create_drive().unwrap(),
            buff: Vec::new(),
            pending_writes: HashMap::new(),
        }
    }

    fn read(&mut self, inode: Inode, offset: usize, size: usize) -> Option<&[u8]> {
        let filename = format!("{}.txt", inode);
        match self.get_file_content(&filename) {
            Ok(data) => {
                self.buff = data[offset..cmp::min(data.len(), offset + size)].to_vec();
                Some(&self.buff)
            }
            Err(e) => {
                error!("Got error: {:?}", e);
                Some(&[])
            }
        }
    }

    fn write(&mut self, inode: Inode, offset: usize, data: &[u8]) {
        if !self.pending_writes.contains_key(&inode) {
            self.pending_writes.insert(inode, Vec::new());
        }

        self.pending_writes
            .entry(inode)
            .or_insert(Vec::new())
            .push(PendingWrite {
                inode,
                offset,
                data: data.to_vec(),
            });
    }

    fn remove(&mut self, inode: Inode) {
        let filename = format!("{}.txt", inode);
        let file_id = self.get_file_id(&filename).unwrap_or_default();
        let result = self.hub
            .files()
            .delete(&file_id)
            .supports_team_drives(false)
            .doit();

        let result = self.hub.files().empty_trash().doit();
    }

    fn flush(&mut self, inode: Inode) {
        let filename = format!("{}.txt", inode);

        if !self.pending_writes.contains_key(&inode) {
            info!("flush() called but there are no pending writes on inode={}. nothing to do, moving on...", inode);
            return;
        }

        let existence = self.file_exists(&filename);
        // debug!("existence: {:?}", existence);

        if existence.is_err() {
            let mut data: Vec<u8> = Vec::new();
            self.apply_pending_writes_on_data(inode, &mut data);

            let dummy_file = DummyFile::new(&data);
            let mut req = drive3::File::default();
            req.name = Some(inode.to_string() + ".txt");
            let result = self.hub
                .files()
                .create(req)
                .use_content_as_indexable_text(true)
                .supports_team_drives(false)
                .ignore_default_visibility(true)
                .upload_resumable(dummy_file, "text/plain".parse().unwrap());

        // debug!("write result: {:#?}", result);
        } else {
            warn!("file already exists! should download + overwrite + upload");
            let mut file_data = self.get_file_content(&filename).unwrap_or_default();
            self.apply_pending_writes_on_data(inode, &mut file_data);
            self.remove(inode); // delete the file from drive
            self.write(inode, 0, &file_data); // create a single pending write containing the final state of the file
            self.flush(inode); // flush that pending write on a freshly created file
        }
    }
}
struct DummyFile {
    cursor: u64,
    data: Vec<u8>,
}

impl DummyFile {
    fn new(data: &[u8]) -> DummyFile {
        DummyFile {
            cursor: 0,
            data: Vec::from(data),
        }
    }
}

impl Seek for DummyFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let position: i64 = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.data.len() as i64 - offset,
            SeekFrom::Current(offset) => self.cursor as i64 + offset,
        };

        if position < 0 {
            Err(io::Error::from(io::ErrorKind::InvalidInput))
        } else {
            self.cursor = position as u64;
            Ok(self.cursor)
        }
    }
}

impl Read for DummyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cursor < self.data.len() as u64 {
            buf[0] = self.data[self.cursor as usize];
            self.cursor += 1;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut written: usize = 0;
        for i in self.cursor..self.data.len() as u64 {
            buf.push(self.data[i as usize]);
            written += 1;
        }
        self.cursor += written as u64;
        Ok(written)
    }
    fn read_to_string(&mut self, buf: &mut String) -> io::Result<usize> {
        let mut written: usize = 0;
        for i in self.cursor..self.data.len() as u64 {
            buf.push(self.data[i as usize] as char);
            written += 1;
        }
        self.cursor += written as u64;
        Ok(written)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        Ok(())
    }
    fn by_ref(&mut self) -> &mut Self
    where
        Self: Sized,
    {
        self
    }
}