use bitflags::bitflags;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_longlong};
use std::ptr;

use crate::key::{RedisKey, RedisKeyWritable};
use crate::raw::{ModuleOptions, Version};
use crate::{add_info_field_long_long, add_info_field_str, raw, utils, Status};
use crate::{add_info_section, LogLevel};
use crate::{RedisError, RedisResult, RedisString, RedisValue};

#[cfg(feature = "experimental-api")]
use std::ffi::CStr;

#[cfg(feature = "experimental-api")]
mod timer;

#[cfg(feature = "experimental-api")]
pub mod thread_safe;

#[cfg(feature = "experimental-api")]
pub mod blocked;

pub mod info;

pub mod keys_cursor;

/// `Context` is a structure that's designed to give us a high-level interface to
/// the Redis module API by abstracting away the raw C FFI calls.
pub struct Context {
    pub ctx: *mut raw::RedisModuleCtx,
}

pub struct StrCallArgs<'a> {
    is_owner: bool,
    args: Vec<*mut raw::RedisModuleString>,
    // Phantom is used to make sure the object will not live longer than actual arguments slice
    phantom: std::marker::PhantomData<&'a raw::RedisModuleString>,
}

impl<'a> Drop for StrCallArgs<'a> {
    fn drop(&mut self) {
        if self.is_owner {
            self.args.iter_mut().for_each(|v| unsafe {
                raw::RedisModule_FreeString.unwrap()(std::ptr::null_mut(), *v)
            });
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> From<&'a [&T]> for StrCallArgs<'a> {
    fn from(vals: &'a [&T]) -> Self {
        StrCallArgs {
            is_owner: true,
            args: vals
                .iter()
                .map(|v| RedisString::create_from_slice(std::ptr::null_mut(), v.as_ref()).take())
                .collect(),
            phantom: std::marker::PhantomData,
        }
    }
}

impl<'a> From<&'a [&RedisString]> for StrCallArgs<'a> {
    fn from(vals: &'a [&RedisString]) -> Self {
        StrCallArgs {
            is_owner: false,
            args: vals.iter().map(|v| v.inner).collect(),
            phantom: std::marker::PhantomData,
        }
    }
}

impl<'a, const SIZE: usize, T: ?Sized> From<&'a [&T; SIZE]> for StrCallArgs<'a>
where
    for<'b> &'a [&'b T]: Into<StrCallArgs<'a>>,
{
    fn from(vals: &'a [&T; SIZE]) -> Self {
        vals.as_ref().into()
    }
}

impl<'a> StrCallArgs<'a> {
    fn args_mut(&mut self) -> &mut [*mut raw::RedisModuleString] {
        &mut self.args
    }
}

impl Context {
    pub const fn new(ctx: *mut raw::RedisModuleCtx) -> Self {
        Self { ctx }
    }

    #[must_use]
    pub const fn dummy() -> Self {
        Self {
            ctx: ptr::null_mut(),
        }
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        crate::logging::log_internal(self.ctx, level, message);
    }

    pub fn log_debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    pub fn log_notice(&self, message: &str) {
        self.log(LogLevel::Notice, message);
    }

    pub fn log_verbose(&self, message: &str) {
        self.log(LogLevel::Verbose, message);
    }

    pub fn log_warning(&self, message: &str) {
        self.log(LogLevel::Warning, message);
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_AutoMemory` is missing in redismodule.h
    pub fn auto_memory(&self) {
        unsafe {
            raw::RedisModule_AutoMemory.unwrap()(self.ctx);
        }
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_IsKeysPositionRequest` is missing in redismodule.h
    #[must_use]
    pub fn is_keys_position_request(&self) -> bool {
        // We want this to be available in tests where we don't have an actual Redis to call
        if cfg!(feature = "test") {
            return false;
        }

        let result = unsafe { raw::RedisModule_IsKeysPositionRequest.unwrap()(self.ctx) };

        result != 0
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_KeyAtPos` is missing in redismodule.h
    pub fn key_at_pos(&self, pos: i32) {
        // TODO: This will crash redis if `pos` is out of range.
        // Think of a way to make this safe by checking the range.
        unsafe {
            raw::RedisModule_KeyAtPos.unwrap()(self.ctx, pos as c_int);
        }
    }

    pub fn call<'a, T: Into<StrCallArgs<'a>>>(&self, command: &str, args: T) -> RedisResult {
        let mut call_args: StrCallArgs = args.into();
        let final_args = call_args.args_mut();

        let cmd = CString::new(command).unwrap();
        let reply: *mut raw::RedisModuleCallReply = unsafe {
            let p_call = raw::RedisModule_Call.unwrap();
            p_call(
                self.ctx,
                cmd.as_ptr(),
                raw::FMT,
                final_args.as_mut_ptr(),
                final_args.len(),
            )
        };
        let result = Self::parse_call_reply(reply);
        if !reply.is_null() {
            raw::free_call_reply(reply);
        }
        result
    }

    fn parse_call_reply(reply: *mut raw::RedisModuleCallReply) -> RedisResult {
        match raw::call_reply_type(reply) {
            raw::ReplyType::Error => Err(RedisError::String(raw::call_reply_string(reply))),
            raw::ReplyType::Unknown => Err(RedisError::Str("Error on method call")),
            raw::ReplyType::Array => {
                let length = raw::call_reply_length(reply);
                let mut vec = Vec::with_capacity(length);
                for i in 0..length {
                    vec.push(Self::parse_call_reply(raw::call_reply_array_element(
                        reply, i,
                    ))?);
                }
                Ok(RedisValue::Array(vec))
            }
            raw::ReplyType::Integer => Ok(RedisValue::Integer(raw::call_reply_integer(reply))),
            raw::ReplyType::String => Ok(RedisValue::SimpleString(raw::call_reply_string(reply))),
            raw::ReplyType::Null => Ok(RedisValue::Null),
        }
    }

    #[must_use]
    pub fn str_as_legal_resp_string(s: &str) -> CString {
        CString::new(
            s.chars()
                .map(|c| match c {
                    '\r' | '\n' | '\0' => b' ',
                    _ => c as u8,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_simple_string(&self, s: &str) -> raw::Status {
        let msg = Self::str_as_legal_resp_string(s);
        unsafe { raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_error_string(&self, s: &str) -> raw::Status {
        let msg = Self::str_as_legal_resp_string(s);
        unsafe { raw::RedisModule_ReplyWithError.unwrap()(self.ctx, msg.as_ptr()).into() }
    }

    /// # Panics
    ///
    /// Will panic if methods used are missing in redismodule.h
    #[allow(clippy::must_use_candidate)]
    pub fn reply(&self, r: RedisResult) -> raw::Status {
        match r {
            Ok(RedisValue::Integer(v)) => unsafe {
                raw::RedisModule_ReplyWithLongLong.unwrap()(self.ctx, v).into()
            },

            Ok(RedisValue::Float(v)) => unsafe {
                raw::RedisModule_ReplyWithDouble.unwrap()(self.ctx, v).into()
            },

            Ok(RedisValue::SimpleStringStatic(s)) => unsafe {
                let msg = CString::new(s).unwrap();
                raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into()
            },

            Ok(RedisValue::SimpleString(s)) => unsafe {
                let msg = CString::new(s).unwrap();
                raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into()
            },

            Ok(RedisValue::BulkString(s)) => unsafe {
                raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                    self.ctx,
                    s.as_ptr().cast::<c_char>(),
                    s.len(),
                )
                .into()
            },

            Ok(RedisValue::BulkRedisString(s)) => unsafe {
                raw::RedisModule_ReplyWithString.unwrap()(self.ctx, s.inner).into()
            },

            Ok(RedisValue::StringBuffer(s)) => unsafe {
                raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                    self.ctx,
                    s.as_ptr().cast::<c_char>(),
                    s.len(),
                )
                .into()
            },

            Ok(RedisValue::Array(array)) => {
                unsafe {
                    // According to the Redis source code this always succeeds,
                    // so there is no point in checking its return value.
                    raw::RedisModule_ReplyWithArray.unwrap()(self.ctx, array.len() as c_long);
                }

                for elem in array {
                    self.reply(Ok(elem));
                }

                raw::Status::Ok
            }

            Ok(RedisValue::Null) => unsafe {
                raw::RedisModule_ReplyWithNull.unwrap()(self.ctx).into()
            },

            Ok(RedisValue::NoReply) => raw::Status::Ok,

            Err(RedisError::WrongArity) => unsafe {
                if self.is_keys_position_request() {
                    // We can't return a result since we don't have a client
                    raw::Status::Err
                } else {
                    raw::RedisModule_WrongArity.unwrap()(self.ctx).into()
                }
            },

            Err(RedisError::WrongType) => {
                self.reply_error_string(RedisError::WrongType.to_string().as_str())
            }

            Err(RedisError::String(s)) => self.reply_error_string(s.as_str()),

            Err(RedisError::Str(s)) => self.reply_error_string(s),
        }
    }

    #[must_use]
    pub fn open_key(&self, key: &RedisString) -> RedisKey {
        RedisKey::open(self.ctx, key)
    }

    #[must_use]
    pub fn open_key_writable(&self, key: &RedisString) -> RedisKeyWritable {
        RedisKeyWritable::open(self.ctx, key)
    }

    pub fn replicate_verbatim(&self) {
        raw::replicate_verbatim(self.ctx);
    }

    #[must_use]
    pub fn create_string(&self, s: &str) -> RedisString {
        RedisString::create(self.ctx, s)
    }

    #[must_use]
    pub const fn get_raw(&self) -> *mut raw::RedisModuleCtx {
        self.ctx
    }

    /// # Safety
    #[cfg(feature = "experimental-api")]
    pub unsafe fn export_shared_api(
        &self,
        func: *const ::std::os::raw::c_void,
        name: *const ::std::os::raw::c_char,
    ) {
        raw::export_shared_api(self.ctx, func, name);
    }

    #[cfg(feature = "experimental-api")]
    #[allow(clippy::must_use_candidate)]
    pub fn notify_keyspace_event(
        &self,
        event_type: raw::NotifyEvent,
        event: &str,
        keyname: &RedisString,
    ) -> raw::Status {
        unsafe { raw::notify_keyspace_event(self.ctx, event_type, event, keyname) }
    }

    #[cfg(feature = "experimental-api")]
    pub fn current_command_name(&self) -> Result<String, RedisError> {
        unsafe {
            match raw::RedisModule_GetCurrentCommandName {
                Some(cmd) => Ok(CStr::from_ptr(cmd(self.ctx)).to_str().unwrap().to_string()),
                None => Err(RedisError::Str(
                    "API RedisModule_GetCurrentCommandName is not available",
                )),
            }
        }
    }

    /// Returns the redis version either by calling `RedisModule_GetServerVersion` API,
    /// Or if it is not available, by calling "info server" API and parsing the reply
    pub fn get_redis_version(&self) -> Result<Version, RedisError> {
        self.get_redis_version_internal(false)
    }

    /// Returns the redis version by calling "info server" API and parsing the reply
    #[cfg(feature = "test")]
    pub fn get_redis_version_rm_call(&self) -> Result<Version, RedisError> {
        self.get_redis_version_internal(true)
    }

    pub fn version_from_info(info: RedisValue) -> Result<Version, RedisError> {
        if let RedisValue::SimpleString(info_str) = info {
            if let Some(ver) = utils::get_regexp_captures(
                info_str.as_str(),
                r"(?m)\bredis_version:([0-9]+)\.([0-9]+)\.([0-9]+)\b",
            ) {
                return Ok(Version {
                    major: ver[0].parse::<c_int>().unwrap(),
                    minor: ver[1].parse::<c_int>().unwrap(),
                    patch: ver[2].parse::<c_int>().unwrap(),
                });
            }
        }
        Err(RedisError::Str("Error getting redis_version"))
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn get_redis_version_internal(&self, force_use_rm_call: bool) -> Result<Version, RedisError> {
        match unsafe { raw::RedisModule_GetServerVersion } {
            Some(api) if !force_use_rm_call => {
                // Call existing API
                Ok(Version::from(unsafe { api() }))
            }
            _ => {
                // Call "info server"
                if let Ok(info) = self.call("info", &["server"]) {
                    Self::version_from_info(info)
                } else {
                    Err(RedisError::Str("Error calling \"info server\""))
                }
            }
        }
    }
    pub fn set_module_options(&self, options: ModuleOptions) {
        unsafe { raw::RedisModule_SetModuleOptions.unwrap()(self.ctx, options.bits()) };
    }

    /// Return ContextFlags object that allows to check properties related to the state of
    /// the current Redis instance such as:
    /// * Role (master/slave)
    /// * Loading RDB/AOF
    /// * Execution mode such as multi exec or Lua
    pub fn get_flags(&self) -> ContextFlags {
        ContextFlags::from_bits_truncate(unsafe {
            raw::RedisModule_GetContextFlags.unwrap()(self.ctx)
        })
    }
}

pub struct InfoContext {
    pub ctx: *mut raw::RedisModuleInfoCtx,
}

impl InfoContext {
    pub const fn new(ctx: *mut raw::RedisModuleInfoCtx) -> Self {
        Self { ctx }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_section(&self, name: Option<&str>) -> Status {
        add_info_section(self.ctx, name)
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_field_str(&self, name: &str, content: &str) -> Status {
        add_info_field_str(self.ctx, name, content)
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_field_long_long(&self, name: &str, value: c_longlong) -> Status {
        add_info_field_long_long(self.ctx, name, value)
    }
}

bitflags! {
    pub struct ContextFlags : c_int {
        /// The command is running in the context of a Lua script
        const LUA = raw::REDISMODULE_CTX_FLAGS_LUA as c_int;

        /// The command is running inside a Redis transaction
        const MULTI = raw::REDISMODULE_CTX_FLAGS_MULTI as c_int;

        /// The instance is a master
        const MASTER = raw::REDISMODULE_CTX_FLAGS_MASTER as c_int;

        /// The instance is a SLAVE
        const SLAVE = raw::REDISMODULE_CTX_FLAGS_SLAVE as c_int;

        /// The instance is read-only (usually meaning it's a slave as well)
        const READONLY = raw::REDISMODULE_CTX_FLAGS_READONLY as c_int;

        /// The instance is running in cluster mode
        const CLUSTER = raw::REDISMODULE_CTX_FLAGS_CLUSTER as c_int;

        /// The instance has AOF enabled
        const AOF = raw::REDISMODULE_CTX_FLAGS_AOF as c_int;

        /// The instance has RDB enabled
        const RDB = raw::REDISMODULE_CTX_FLAGS_RDB as c_int;

        /// The instance has Maxmemory set
        const MAXMEMORY = raw::REDISMODULE_CTX_FLAGS_MAXMEMORY as c_int;

        /// Maxmemory is set and has an eviction policy that may delete keys
        const EVICTED = raw::REDISMODULE_CTX_FLAGS_EVICT as c_int;

        /// Redis is out of memory according to the maxmemory flag.
        const OOM = raw::REDISMODULE_CTX_FLAGS_OOM as c_int;

        /// Less than 25% of memory available according to maxmemory.
        const OOM_WARNING = raw::REDISMODULE_CTX_FLAGS_OOM_WARNING as c_int;

        /// The command was sent over the replication link.
        const REPLICATED = raw::REDISMODULE_CTX_FLAGS_REPLICATED as c_int;

        /// Redis is currently loading either from AOF or RDB.
        const LOADING = raw::REDISMODULE_CTX_FLAGS_LOADING as c_int;

        /// The replica has no link with its master
        const REPLICA_IS_STALE = raw::REDISMODULE_CTX_FLAGS_REPLICA_IS_STALE as c_int;

        /// The replica is trying to connect with the master
        const REPLICA_IS_CONNECTING = raw::REDISMODULE_CTX_FLAGS_REPLICA_IS_CONNECTING as c_int;

        /// The replica is receiving an RDB file from its master.
        const REPLICA_IS_TRANSFERRING = raw::REDISMODULE_CTX_FLAGS_REPLICA_IS_TRANSFERRING as c_int;

        /// The replica is online, receiving updates from its master
        const REPLICA_IS_ONLINE = raw::REDISMODULE_CTX_FLAGS_REPLICA_IS_ONLINE as c_int;

        /// There is currently some background process active.
        const ACTIVE_CHILD = raw::REDISMODULE_CTX_FLAGS_ACTIVE_CHILD as c_int;

        /// Redis is currently running inside background child process.
        const IS_CHILD = raw::REDISMODULE_CTX_FLAGS_IS_CHILD as c_int;

        /// The next EXEC will fail due to dirty CAS (touched keys).
        const MULTI_DIRTY = raw::REDISMODULE_CTX_FLAGS_MULTI_DIRTY as c_int;

        /// The current client does not allow blocking, either called from
        /// within multi, lua, or from another module using RM_Call
        const DENY_BLOCKING = raw::REDISMODULE_CTX_FLAGS_DENY_BLOCKING as c_int;

        /// The current client uses RESP3 protocol
        const FLAGS_RESP3 = raw::REDISMODULE_CTX_FLAGS_RESP3 as c_int;

        /// Redis is currently async loading database for diskless replication.
        const ASYNC_LOADING = raw::REDISMODULE_CTX_FLAGS_ASYNC_LOADING as c_int;
    }
}
