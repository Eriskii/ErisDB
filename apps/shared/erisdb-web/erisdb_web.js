let ErisDBWeb = (function(exports) {
    let script_src;
    if (typeof document !== 'undefined' && document.currentScript !== null) {
        script_src = new URL(document.currentScript.src, location.href).toString();
    }

    /**
     * A change feed: each `next()` is the next change, or `null` once the feed has ended.
     */
    class Changes {
        static __wrap(ptr) {
            const obj = Object.create(Changes.prototype);
            obj.__wbg_ptr = ptr;
            ChangesFinalization.register(obj, obj.__wbg_ptr, obj);
            return obj;
        }
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            ChangesFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_changes_free(ptr, 0);
        }
        /**
         * Stop following: the stream closes, and a waiting `next()` resolves to `null`.
         */
        close() {
            wasm.changes_close(this.__wbg_ptr);
        }
        /**
         * The next change row, as ErisDB sent it; `null` once the feed has ended, as it does when
         * ErisDB stops letting the app in. Throws `unreachable` when the stream broke.
         * @returns {Promise<any>}
         */
        next() {
            const ret = wasm.changes_next(this.__wbg_ptr);
            return ret;
        }
    }
    if (Symbol.dispose) Changes.prototype[Symbol.dispose] = Changes.prototype.free;
    exports.Changes = Changes;

    /**
     * The app's connection to its mesh's ErisDB.
     */
    class Client {
        static __wrap(ptr) {
            const obj = Object.create(Client.prototype);
            obj.__wbg_ptr = ptr;
            ClientFinalization.register(obj, obj.__wbg_ptr, obj);
            return obj;
        }
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            ClientFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_client_free(ptr, 0);
        }
        /**
         * Follow the change feed from `since` (exclusive), narrowed to `facet` when given:
         * `/v1/changes/stream`. Resolves once ErisDB opens it; throws `refused`, with the `status`
         * and `body` ErisDB answered, when it will not.
         * @param {number} since
         * @param {string | null} [facet]
         * @returns {Promise<any>}
         */
        changes(since, facet) {
            var ptr0 = isLikeNone(facet) ? 0 : passStringToWasm0(facet, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            var len0 = WASM_VECTOR_LEN;
            const ret = wasm.client_changes(this.__wbg_ptr, since, ptr0, len0);
            return ret;
        }
        /**
         * Hang up on ErisDB. The next request dials again.
         */
        close() {
            wasm.client_close(this.__wbg_ptr);
        }
        /**
         * One API call: `{status, body}`, the body JSON where it is JSON. `body`, when given, goes
         * as JSON. Throws `unreachable` when ErisDB could not be asked, or did not answer.
         * @param {string} method
         * @param {string} path
         * @param {any} body
         * @returns {Promise<any>}
         */
        request(method, path, body) {
            const ptr0 = passStringToWasm0(method, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            const ret = wasm.client_request(this.__wbg_ptr, ptr0, len0, ptr1, len1, body);
            return ret;
        }
        /**
         * ErisDB's endpoint id.
         * @returns {string}
         */
        get server() {
            let deferred1_0;
            let deferred1_1;
            try {
                const ret = wasm.client_server(this.__wbg_ptr);
                deferred1_0 = ret[0];
                deferred1_1 = ret[1];
                return getStringFromWasm0(ret[0], ret[1]);
            } finally {
                wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
            }
        }
    }
    if (Symbol.dispose) Client.prototype[Symbol.dispose] = Client.prototype.free;
    exports.Client = Client;

    class IntoUnderlyingByteSource {
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            IntoUnderlyingByteSourceFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_intounderlyingbytesource_free(ptr, 0);
        }
        /**
         * @returns {number}
         */
        get autoAllocateChunkSize() {
            const ret = wasm.intounderlyingbytesource_autoAllocateChunkSize(this.__wbg_ptr);
            return ret >>> 0;
        }
        cancel() {
            const ptr = this.__destroy_into_raw();
            wasm.intounderlyingbytesource_cancel(ptr);
        }
        /**
         * @param {ReadableByteStreamController} controller
         * @returns {Promise<any>}
         */
        pull(controller) {
            const ret = wasm.intounderlyingbytesource_pull(this.__wbg_ptr, controller);
            return ret;
        }
        /**
         * @param {ReadableByteStreamController} controller
         */
        start(controller) {
            wasm.intounderlyingbytesource_start(this.__wbg_ptr, controller);
        }
        /**
         * @returns {ReadableStreamType}
         */
        get type() {
            const ret = wasm.intounderlyingbytesource_type(this.__wbg_ptr);
            return __wbindgen_enum_ReadableStreamType[ret];
        }
    }
    if (Symbol.dispose) IntoUnderlyingByteSource.prototype[Symbol.dispose] = IntoUnderlyingByteSource.prototype.free;
    exports.IntoUnderlyingByteSource = IntoUnderlyingByteSource;

    class IntoUnderlyingSink {
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            IntoUnderlyingSinkFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_intounderlyingsink_free(ptr, 0);
        }
        /**
         * @param {any} reason
         * @returns {Promise<any>}
         */
        abort(reason) {
            const ptr = this.__destroy_into_raw();
            const ret = wasm.intounderlyingsink_abort(ptr, reason);
            return ret;
        }
        /**
         * @returns {Promise<any>}
         */
        close() {
            const ptr = this.__destroy_into_raw();
            const ret = wasm.intounderlyingsink_close(ptr);
            return ret;
        }
        /**
         * @param {any} chunk
         * @returns {Promise<any>}
         */
        write(chunk) {
            const ret = wasm.intounderlyingsink_write(this.__wbg_ptr, chunk);
            return ret;
        }
    }
    if (Symbol.dispose) IntoUnderlyingSink.prototype[Symbol.dispose] = IntoUnderlyingSink.prototype.free;
    exports.IntoUnderlyingSink = IntoUnderlyingSink;

    class IntoUnderlyingSource {
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            IntoUnderlyingSourceFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_intounderlyingsource_free(ptr, 0);
        }
        cancel() {
            const ptr = this.__destroy_into_raw();
            wasm.intounderlyingsource_cancel(ptr);
        }
        /**
         * @param {ReadableStreamDefaultController} controller
         * @returns {Promise<any>}
         */
        pull(controller) {
            const ret = wasm.intounderlyingsource_pull(this.__wbg_ptr, controller);
            return ret;
        }
    }
    if (Symbol.dispose) IntoUnderlyingSource.prototype[Symbol.dispose] = IntoUnderlyingSource.prototype.free;
    exports.IntoUnderlyingSource = IntoUnderlyingSource;

    /**
     * A sign-in under way: the request is with the person on the device.
     */
    class Joining {
        static __wrap(ptr) {
            const obj = Object.create(Joining.prototype);
            obj.__wbg_ptr = ptr;
            JoiningFinalization.register(obj, obj.__wbg_ptr, obj);
            return obj;
        }
        __destroy_into_raw() {
            const ptr = this.__wbg_ptr;
            this.__wbg_ptr = 0;
            JoiningFinalization.unregister(this);
            return ptr;
        }
        free() {
            const ptr = this.__destroy_into_raw();
            wasm.__wbg_joining_free(ptr, 0);
        }
        /**
         * The person's answer: `{note, snapshot}` once they approved and ErisDB enrolled the app,
         * or `null` when they said no. Throws `expired` once the ticket ends unanswered.
         * @returns {Promise<any>}
         */
        approval() {
            const ret = wasm.joining_approval(this.__wbg_ptr);
            return ret;
        }
        /**
         * The request's fingerprint, computed on this page: the person compares it with the one
         * the device shows before approving.
         * @returns {string}
         */
        get fingerprint() {
            let deferred1_0;
            let deferred1_1;
            try {
                const ret = wasm.joining_fingerprint(this.__wbg_ptr);
                deferred1_0 = ret[0];
                deferred1_1 = ret[1];
                return getStringFromWasm0(ret[0], ret[1]);
            } finally {
                wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
            }
        }
        /**
         * The device's name for itself.
         * @returns {string}
         */
        get host() {
            let deferred1_0;
            let deferred1_1;
            try {
                const ret = wasm.joining_host(this.__wbg_ptr);
                deferred1_0 = ret[0];
                deferred1_1 = ret[1];
                return getStringFromWasm0(ret[0], ret[1]);
            } finally {
                wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
            }
        }
        /**
         * What to keep to wait for this answer on a later page: give it to `resumeSignIn`.
         * @returns {any}
         */
        get saved() {
            const ret = wasm.joining_saved(this.__wbg_ptr);
            if (ret[2]) {
                throw takeFromExternrefTable0(ret[1]);
            }
            return takeFromExternrefTable0(ret[0]);
        }
        /**
         * Take the request back: the device forgets it.
         * @returns {Promise<any>}
         */
        withdraw() {
            const ret = wasm.joining_withdraw(this.__wbg_ptr);
            return ret;
        }
    }
    if (Symbol.dispose) Joining.prototype[Symbol.dispose] = Joining.prototype.free;
    exports.Joining = Joining;

    /**
     * The endpoint id `key` calls as, in hex: what ErisAuth and ErisDB know the app by.
     * @param {Uint8Array} key
     * @returns {string}
     */
    function appId(key) {
        let deferred3_0;
        let deferred3_1;
        try {
            const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
            const len0 = WASM_VECTOR_LEN;
            const ret = wasm.appId(ptr0, len0);
            var ptr2 = ret[0];
            var len2 = ret[1];
            if (ret[3]) {
                ptr2 = 0; len2 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
        }
    }
    exports.appId = appId;

    /**
     * Close the endpoint `key` runs on, and every connection on it.
     * @param {Uint8Array} key
     * @returns {Promise<any>}
     */
    function closeEndpoint(key) {
        const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.closeEndpoint(ptr0, len0);
        return ret;
    }
    exports.closeEndpoint = closeEndpoint;

    /**
     * Connect to the ErisDB in `snapshot` as the app holding `key`, stamping `name` into every
     * write's source. ErisDB is the app approved to serve `erisdb`; the snapshot says which relay
     * reaches it. Dialing happens with the first request.
     * @param {Uint8Array} key
     * @param {any} snapshot
     * @param {string} name
     * @returns {Promise<any>}
     */
    function connect(key, snapshot, name) {
        const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.connect(ptr0, len0, snapshot, ptr1, len1);
        return ret;
    }
    exports.connect = connect;

    /**
     * A new key for an app: 32 bytes for the page to keep, the same every time it loads.
     * @returns {Uint8Array}
     */
    function generateKey() {
        const ret = wasm.generateKey();
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    exports.generateKey = generateKey;

    /**
     * Read an `eris://pair/` ticket: `{device, name, relay}`. Throws `bad_ticket` or `no_relay`.
     * @param {string} text
     * @returns {any}
     */
    function readTicket(text) {
        const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.readTicket(ptr0, len0);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return takeFromExternrefTable0(ret[0]);
    }
    exports.readTicket = readTicket;

    /**
     * Wait again for the answer to a sign-in an earlier page made under `key`, from what its
     * `Joining.saved` held.
     * @param {Uint8Array} key
     * @param {any} saved
     * @returns {Promise<any>}
     */
    function resumeSignIn(key, saved) {
        const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.resumeSignIn(ptr0, len0, saved);
        return ret;
    }
    exports.resumeSignIn = resumeSignIn;

    /**
     * Sign in by `ticket` as the app holding `key`, called `name`, asking for `grants` in ErisDB.
     * Resolves to a `Joining` once the device has the request.
     * @param {Uint8Array} key
     * @param {string} ticket_text
     * @param {string} name
     * @param {string[]} grants
     * @returns {Promise<any>}
     */
    function signIn(key, ticket_text, name, grants) {
        const ptr0 = passArray8ToWasm0(key, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(ticket_text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(name, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len2 = WASM_VECTOR_LEN;
        const ptr3 = passArrayJsValueToWasm0(grants, wasm.__wbindgen_malloc);
        const len3 = WASM_VECTOR_LEN;
        const ret = wasm.signIn(ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3);
        return ret;
    }
    exports.signIn = signIn;

    function start() {
        wasm.start();
    }
    exports.start = start;
    function __wbg_get_imports() {
        const import0 = {
            __proto__: null,
            __wbg___wbindgen_boolean_get_7a12af2b3f899c5a: function(arg0) {
                const v = arg0;
                const ret = typeof(v) === 'boolean' ? v : undefined;
                return isLikeNone(ret) ? 0xFFFFFF : ret ? 1 : 0;
            },
            __wbg___wbindgen_debug_string_0e68cf47c9cbd9b0: function(arg0, arg1) {
                const ret = debugString(arg1);
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg___wbindgen_is_function_fcda5e3902d732fe: function(arg0) {
                const ret = typeof(arg0) === 'function';
                return ret;
            },
            __wbg___wbindgen_is_null_5160b3e381865372: function(arg0) {
                const ret = arg0 === null;
                return ret;
            },
            __wbg___wbindgen_is_object_edb6b15aa3afe12e: function(arg0) {
                const val = arg0;
                const ret = typeof(val) === 'object' && val !== null;
                return ret;
            },
            __wbg___wbindgen_is_string_c4f7cb494a2a21f1: function(arg0) {
                const ret = typeof(arg0) === 'string';
                return ret;
            },
            __wbg___wbindgen_is_undefined_8c687d0b90d5b524: function(arg0) {
                const ret = arg0 === undefined;
                return ret;
            },
            __wbg___wbindgen_string_get_92ab86bb19cbc12f: function(arg0, arg1) {
                const obj = arg1;
                const ret = typeof(obj) === 'string' ? obj : undefined;
                var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                var len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg___wbindgen_throw_5d9e815e6fdf150f: function(arg0, arg1) {
                throw new Error(getStringFromWasm0(arg0, arg1));
            },
            __wbg__wbg_cb_unref_997e73d32238e655: function(arg0) {
                arg0._wbg_cb_unref();
            },
            __wbg_abort_03f8c5804715cb9a: function(arg0, arg1) {
                arg0.abort(arg1);
            },
            __wbg_abort_76dea11221098ae5: function(arg0) {
                arg0.abort();
            },
            __wbg_addEventListener_ca7d80a9d782e9ac: function() { return handleError(function (arg0, arg1, arg2, arg3) {
                arg0.addEventListener(getStringFromWasm0(arg1, arg2), arg3);
            }, arguments); },
            __wbg_append_8e87c13f0f08f8cd: function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
                arg0.append(getStringFromWasm0(arg1, arg2), getStringFromWasm0(arg3, arg4));
            }, arguments); },
            __wbg_body_9ee03724e9ad0639: function(arg0) {
                const ret = arg0.body;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_buffer_4a989bded7035f57: function(arg0) {
                const ret = arg0.buffer;
                return ret;
            },
            __wbg_byobRequest_f161fc37241dd3d4: function(arg0) {
                const ret = arg0.byobRequest;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_byteLength_0ddb1795e2c7e689: function(arg0) {
                const ret = arg0.byteLength;
                return ret;
            },
            __wbg_byteOffset_46eb015f52b6ad7d: function(arg0) {
                const ret = arg0.byteOffset;
                return ret;
            },
            __wbg_call_6bcf8d3e20937e46: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = arg0.call(arg1, arg2);
                return ret;
            }, arguments); },
            __wbg_cancel_c076350a7e8954be: function(arg0) {
                const ret = arg0.cancel();
                return ret;
            },
            __wbg_catch_e2134ef6dcdb69ed: function(arg0, arg1) {
                const ret = arg0.catch(arg1);
                return ret;
            },
            __wbg_changes_new: function(arg0) {
                const ret = Changes.__wrap(arg0);
                return ret;
            },
            __wbg_clearTimeout_1daad5c6ee4fcd3b: function(arg0) {
                const ret = clearTimeout(arg0);
                return ret;
            },
            __wbg_clearTimeout_47a40e3be01ed7a3: function() { return handleError(function (arg0, arg1) {
                arg0.clearTimeout(arg1);
            }, arguments); },
            __wbg_client_new: function(arg0) {
                const ret = Client.__wrap(arg0);
                return ret;
            },
            __wbg_close_22882088c136df25: function() { return handleError(function (arg0) {
                arg0.close();
            }, arguments); },
            __wbg_close_8b609460dd26e367: function() { return handleError(function (arg0) {
                arg0.close();
            }, arguments); },
            __wbg_close_a009f540ff811039: function() { return handleError(function (arg0) {
                arg0.close();
            }, arguments); },
            __wbg_code_0589939084228780: function(arg0) {
                const ret = arg0.code;
                return ret;
            },
            __wbg_code_6bc89190cbffee9e: function(arg0) {
                const ret = arg0.code;
                return ret;
            },
            __wbg_crypto_38df2bab126b63dc: function(arg0) {
                const ret = arg0.crypto;
                return ret;
            },
            __wbg_data_aa7ba49b34c0d883: function(arg0) {
                const ret = arg0.data;
                return ret;
            },
            __wbg_done_cffed884d87aa22e: function(arg0) {
                const ret = arg0.done;
                return ret;
            },
            __wbg_encodeURIComponent_d4884d77d1dfa553: function(arg0, arg1) {
                const ret = encodeURIComponent(getStringFromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_enqueue_6cb545d22db14f33: function() { return handleError(function (arg0, arg1) {
                arg0.enqueue(arg1);
            }, arguments); },
            __wbg_entries_f9de04c09a57314e: function(arg0) {
                const ret = arg0.entries();
                return ret;
            },
            __wbg_error_757e9472f8410341: function(arg0, arg1) {
                let deferred0_0;
                let deferred0_1;
                try {
                    deferred0_0 = arg0;
                    deferred0_1 = arg1;
                    console.error(getStringFromWasm0(arg0, arg1));
                } finally {
                    wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
                }
            },
            __wbg_fetch_7a37768ab67c5bd8: function(arg0) {
                const ret = fetch(arg0);
                return ret;
            },
            __wbg_fetch_920242d59bac2026: function(arg0, arg1) {
                const ret = arg0.fetch(arg1);
                return ret;
            },
            __wbg_getRandomValues_436a51d0629d84e1: function() { return handleError(function (arg0, arg1) {
                globalThis.crypto.getRandomValues(getArrayU8FromWasm0(arg0, arg1));
            }, arguments); },
            __wbg_getRandomValues_c44a50d8cfdaebeb: function() { return handleError(function (arg0, arg1) {
                arg0.getRandomValues(arg1);
            }, arguments); },
            __wbg_getReader_9facd4f899beac89: function() { return handleError(function (arg0) {
                const ret = arg0.getReader();
                return ret;
            }, arguments); },
            __wbg_get_989d0a1309644f2b: function() { return handleError(function (arg0, arg1) {
                const ret = Reflect.get(arg0, arg1);
                return ret;
            }, arguments); },
            __wbg_get_b1f0ab13c737f856: function(arg0, arg1) {
                const ret = arg0[arg1 >>> 0];
                return ret;
            },
            __wbg_get_done_a668aa62d81fad70: function(arg0) {
                const ret = arg0.done;
                return isLikeNone(ret) ? 0xFFFFFF : ret ? 1 : 0;
            },
            __wbg_get_value_6c62a77c168d825a: function(arg0) {
                const ret = arg0.value;
                return ret;
            },
            __wbg_has_464f8b9416279d65: function() { return handleError(function (arg0, arg1) {
                const ret = Reflect.has(arg0, arg1);
                return ret;
            }, arguments); },
            __wbg_headers_2594464ff62969f3: function(arg0) {
                const ret = arg0.headers;
                return ret;
            },
            __wbg_instanceof_ArrayBuffer_d4ff01f8247925ae: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof ArrayBuffer;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_instanceof_Blob_38df052cf775f822: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof Blob;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_instanceof_Response_6366a785e400039b: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof Response;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_isArray_5674713bb7b79043: function(arg0) {
                const ret = Array.isArray(arg0);
                return ret;
            },
            __wbg_joining_new: function(arg0) {
                const ret = Joining.__wrap(arg0);
                return ret;
            },
            __wbg_length_31bdaf014f5fbde2: function(arg0) {
                const ret = arg0.length;
                return ret;
            },
            __wbg_message_d6ab3d8a5aedce1c: function(arg0, arg1) {
                const ret = arg1.message;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_msCrypto_bd5a034af96bcba6: function(arg0) {
                const ret = arg0.msCrypto;
                return ret;
            },
            __wbg_new_0afe64b4dc16ab74: function() { return handleError(function () {
                const ret = new Headers();
                return ret;
            }, arguments); },
            __wbg_new_1da3429bc3c4541c: function(arg0) {
                const ret = new Uint8Array(arg0);
                return ret;
            },
            __wbg_new_227d7c05414eb861: function() {
                const ret = new Error();
                return ret;
            },
            __wbg_new_82926b2a4d30e175: function() { return handleError(function (arg0, arg1) {
                const ret = new WebSocket(getStringFromWasm0(arg0, arg1));
                return ret;
            }, arguments); },
            __wbg_new_a32a1ab6c6655abe: function(arg0, arg1) {
                const ret = new Error(getStringFromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_new_baa0a0207935dd43: function() { return handleError(function () {
                const ret = new AbortController();
                return ret;
            }, arguments); },
            __wbg_new_bebc3f4757acf305: function() {
                const ret = new Object();
                return ret;
            },
            __wbg_new_ffa92086ea89f79c: function() {
                const ret = new Array();
                return ret;
            },
            __wbg_new_from_slice_4ee02165f9de919e: function(arg0, arg1) {
                const ret = new Uint8Array(getArrayU8FromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_new_typed_6f8b0d724fe26c07: function(arg0, arg1) {
                try {
                    var state0 = {a: arg0, b: arg1};
                    var cb0 = (arg0, arg1) => {
                        const a = state0.a;
                        state0.a = 0;
                        try {
                            return wasm_bindgen__convert__closures_____invoke__h17a4e3fd3c438098(a, state0.b, arg0, arg1);
                        } finally {
                            state0.a = a;
                        }
                    };
                    const ret = new Promise(cb0);
                    return ret;
                } finally {
                    state0.a = 0;
                }
            },
            __wbg_new_with_byte_offset_and_length_492c969e8b5da8a4: function(arg0, arg1, arg2) {
                const ret = new Uint8Array(arg0, arg1 >>> 0, arg2 >>> 0);
                return ret;
            },
            __wbg_new_with_length_5ffeddb9d9fbb96f: function(arg0) {
                const ret = new Uint8Array(arg0 >>> 0);
                return ret;
            },
            __wbg_new_with_str_and_init_2f31a77deda127fd: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = new Request(getStringFromWasm0(arg0, arg1), arg2);
                return ret;
            }, arguments); },
            __wbg_new_with_str_sequence_9ab4875494df9d99: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = new WebSocket(getStringFromWasm0(arg0, arg1), arg2);
                return ret;
            }, arguments); },
            __wbg_next_f31ecb8646d2c605: function() { return handleError(function (arg0) {
                const ret = arg0.next();
                return ret;
            }, arguments); },
            __wbg_node_84ea875411254db1: function(arg0) {
                const ret = arg0.node;
                return ret;
            },
            __wbg_now_d1fb6650485d7f3e: function() {
                const ret = Date.now();
                return ret;
            },
            __wbg_now_e7c6795a7f81e10f: function(arg0) {
                const ret = arg0.now();
                return ret;
            },
            __wbg_parse_6937a9050adfb0e1: function() { return handleError(function (arg0, arg1) {
                const ret = JSON.parse(getStringFromWasm0(arg0, arg1));
                return ret;
            }, arguments); },
            __wbg_performance_3fcf6e32a7e1ed0a: function(arg0) {
                const ret = arg0.performance;
                return ret;
            },
            __wbg_process_44c7a14e11e9f69e: function(arg0) {
                const ret = arg0.process;
                return ret;
            },
            __wbg_protocol_b76b26f89befc411: function(arg0, arg1) {
                const ret = arg1.protocol;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_prototypesetcall_ae9f5e7459250748: function(arg0, arg1, arg2) {
                Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
            },
            __wbg_push_bfdf956ba476f65b: function(arg0, arg1) {
                const ret = arg0.push(arg1);
                return ret;
            },
            __wbg_queueMicrotask_85c90f6987555d65: function(arg0) {
                const ret = arg0.queueMicrotask;
                return ret;
            },
            __wbg_queueMicrotask_f6a1fa10b81d1fc0: function(arg0) {
                queueMicrotask(arg0);
            },
            __wbg_randomFillSync_6c25eac9869eb53c: function() { return handleError(function (arg0, arg1) {
                arg0.randomFillSync(arg1);
            }, arguments); },
            __wbg_read_31091533ffadf971: function(arg0) {
                const ret = arg0.read();
                return ret;
            },
            __wbg_readyState_83df235b27cf257f: function(arg0) {
                const ret = arg0.readyState;
                return ret;
            },
            __wbg_reason_9b593bf440970929: function(arg0, arg1) {
                const ret = arg1.reason;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_releaseLock_1538945f5f183d9f: function(arg0) {
                arg0.releaseLock();
            },
            __wbg_removeEventListener_a14fb03179f66dcd: function() { return handleError(function (arg0, arg1, arg2, arg3) {
                arg0.removeEventListener(getStringFromWasm0(arg1, arg2), arg3);
            }, arguments); },
            __wbg_require_b4edbdcf3e2a1ef0: function() { return handleError(function () {
                const ret = module.require;
                return ret;
            }, arguments); },
            __wbg_resolve_35ec7e0c6af4c82c: function(arg0) {
                const ret = Promise.resolve(arg0);
                return ret;
            },
            __wbg_respond_83a71686e927ca32: function() { return handleError(function (arg0, arg1) {
                arg0.respond(arg1 >>> 0);
            }, arguments); },
            __wbg_send_af811d92e7aea5c4: function() { return handleError(function (arg0, arg1, arg2) {
                arg0.send(getArrayU8FromWasm0(arg1, arg2));
            }, arguments); },
            __wbg_setTimeout_6613a51400c1bf9f: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = arg0.setTimeout(arg1, arg2);
                return ret;
            }, arguments); },
            __wbg_setTimeout_b4a9096784635514: function(arg0, arg1) {
                const ret = setTimeout(arg0, arg1);
                return ret;
            },
            __wbg_set_5f2ad37e5e02dc7b: function(arg0, arg1, arg2) {
                arg0.set(getArrayU8FromWasm0(arg1, arg2));
            },
            __wbg_set_a377297433dfea63: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = Reflect.set(arg0, arg1, arg2);
                return ret;
            }, arguments); },
            __wbg_set_binaryType_f88804219cea0f9e: function(arg0, arg1) {
                arg0.binaryType = __wbindgen_enum_BinaryType[arg1];
            },
            __wbg_set_body_f39cee72c74a5b02: function(arg0, arg1) {
                arg0.body = arg1;
            },
            __wbg_set_cache_6e8c35a5ed76edb1: function(arg0, arg1) {
                arg0.cache = __wbindgen_enum_RequestCache[arg1];
            },
            __wbg_set_credentials_bf1d3e014213a571: function(arg0, arg1) {
                arg0.credentials = __wbindgen_enum_RequestCredentials[arg1];
            },
            __wbg_set_handle_event_8e892f11ab732c99: function(arg0, arg1) {
                arg0.handleEvent = arg1;
            },
            __wbg_set_headers_31d373f68f28bf76: function(arg0, arg1) {
                arg0.headers = arg1;
            },
            __wbg_set_method_82e6c3c083da0734: function(arg0, arg1, arg2) {
                arg0.method = getStringFromWasm0(arg1, arg2);
            },
            __wbg_set_mode_f7e56ed768d0ade0: function(arg0, arg1) {
                arg0.mode = __wbindgen_enum_RequestMode[arg1];
            },
            __wbg_set_onclose_97aab74a1c2662f7: function(arg0, arg1) {
                arg0.onclose = arg1;
            },
            __wbg_set_onerror_88cb2134dca3aba2: function(arg0, arg1) {
                arg0.onerror = arg1;
            },
            __wbg_set_onmessage_0f0d391465909a45: function(arg0, arg1) {
                arg0.onmessage = arg1;
            },
            __wbg_set_onopen_e91e64af99b7966d: function(arg0, arg1) {
                arg0.onopen = arg1;
            },
            __wbg_set_referrer_13cf56bbccbb2ceb: function(arg0, arg1, arg2) {
                arg0.referrer = getStringFromWasm0(arg1, arg2);
            },
            __wbg_set_referrer_policy_dac1caa6e937479a: function(arg0, arg1) {
                arg0.referrerPolicy = __wbindgen_enum_ReferrerPolicy[arg1];
            },
            __wbg_set_signal_f4b32f40f19786f4: function(arg0, arg1) {
                arg0.signal = arg1;
            },
            __wbg_signal_45367c6c2255877a: function(arg0) {
                const ret = arg0.signal;
                return ret;
            },
            __wbg_stack_3b0d974bbf31e44f: function(arg0, arg1) {
                const ret = arg1.stack;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_static_accessor_GLOBAL_8eb4cd83130a11a0: function() {
                const ret = typeof global === 'undefined' ? null : global;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_GLOBAL_THIS_1e7044f654e934db: function() {
                const ret = typeof globalThis === 'undefined' ? null : globalThis;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_SELF_d8b50611246a6d92: function() {
                const ret = typeof self === 'undefined' ? null : self;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_WINDOW_fd0bc376bf0f8b42: function() {
                const ret = typeof window === 'undefined' ? null : window;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_status_88ccc72fe7bea621: function(arg0) {
                const ret = arg0.status;
                return ret;
            },
            __wbg_stringify_54b3d9b61602aee6: function() { return handleError(function (arg0) {
                const ret = JSON.stringify(arg0);
                return ret;
            }, arguments); },
            __wbg_subarray_1daff70dde20c145: function(arg0, arg1, arg2) {
                const ret = arg0.subarray(arg1 >>> 0, arg2 >>> 0);
                return ret;
            },
            __wbg_then_7a850dae4493f353: function(arg0, arg1, arg2) {
                const ret = arg0.then(arg1, arg2);
                return ret;
            },
            __wbg_then_b830475380919203: function(arg0, arg1) {
                const ret = arg0.then(arg1);
                return ret;
            },
            __wbg_url_f1948b0d7f881e02: function(arg0, arg1) {
                const ret = arg1.url;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_value_c227f843d21da141: function(arg0) {
                const ret = arg0.value;
                return ret;
            },
            __wbg_versions_276b2795b1c6a219: function(arg0) {
                const ret = arg0.versions;
                return ret;
            },
            __wbg_view_d8c7b26e4d4650f1: function(arg0) {
                const ret = arg0.view;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_wasClean_a32ef5f1fd90161e: function(arg0) {
                const ret = arg0.wasClean;
                return ret;
            },
            __wbindgen_generic_0000000000000001: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 151, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f);
                return ret;
            },
            __wbindgen_generic_0000000000000002: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 375, ret: Result(Unit), inner_ret: Some(Result(Unit)) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__hb327e46a3fc50be7);
                return ret;
            },
            __wbindgen_generic_0000000000000003: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("CloseEvent")], shim_idx: 151, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_32);
                return ret;
            },
            __wbindgen_generic_0000000000000004: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 151, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_33);
                return ret;
            },
            __wbindgen_generic_0000000000000005: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [], shim_idx: 114, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h06d79f93eacd1766);
                return ret;
            },
            __wbindgen_generic_0000000000000006: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [], shim_idx: 59, ret: Unit, inner_ret: Some(Unit) }, mutable: false }) -> Externref`.
                const ret = makeClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h16735b61b2cd8b04);
                return ret;
            },
            __wbindgen_generic_0000000000000007: function(arg0) {
                // Cast intrinsic for `F64 -> Externref`.
                const ret = arg0;
                return ret;
            },
            __wbindgen_generic_0000000000000008: function(arg0, arg1) {
                // Cast intrinsic for `Ref(Slice(U8)) -> NamedExternref("Uint8Array")`.
                const ret = getArrayU8FromWasm0(arg0, arg1);
                return ret;
            },
            __wbindgen_generic_0000000000000009: function(arg0, arg1) {
                // Cast intrinsic for `Ref(String) -> Externref`.
                const ret = getStringFromWasm0(arg0, arg1);
                return ret;
            },
            __wbindgen_init_externref_table: function() {
                const table = wasm.__wbindgen_externrefs;
                const offset = table.grow(4);
                table.set(0, undefined);
                table.set(offset + 0, undefined);
                table.set(offset + 1, null);
                table.set(offset + 2, true);
                table.set(offset + 3, false);
            },
        };
        return {
            __proto__: null,
            "./erisdb_web_bg.js": import0,
        };
    }

    function wasm_bindgen__convert__closures_____invoke__h06d79f93eacd1766(arg0, arg1) {
        wasm.wasm_bindgen__convert__closures_____invoke__h06d79f93eacd1766(arg0, arg1);
    }

    function wasm_bindgen__convert__closures_____invoke__h16735b61b2cd8b04(arg0, arg1) {
        wasm.wasm_bindgen__convert__closures_____invoke__h16735b61b2cd8b04(arg0, arg1);
    }

    function wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f(arg0, arg1, arg2) {
        wasm.wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f(arg0, arg1, arg2);
    }

    function wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_32(arg0, arg1, arg2) {
        wasm.wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_32(arg0, arg1, arg2);
    }

    function wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_33(arg0, arg1, arg2) {
        wasm.wasm_bindgen__convert__closures_____invoke__h8e98b05756c5df9f_33(arg0, arg1, arg2);
    }

    function wasm_bindgen__convert__closures_____invoke__hb327e46a3fc50be7(arg0, arg1, arg2) {
        const ret = wasm.wasm_bindgen__convert__closures_____invoke__hb327e46a3fc50be7(arg0, arg1, arg2);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }

    function wasm_bindgen__convert__closures_____invoke__h17a4e3fd3c438098(arg0, arg1, arg2, arg3) {
        wasm.wasm_bindgen__convert__closures_____invoke__h17a4e3fd3c438098(arg0, arg1, arg2, arg3);
    }


    const __wbindgen_enum_BinaryType = ["blob", "arraybuffer"];


    const __wbindgen_enum_ReadableStreamType = ["bytes"];


    const __wbindgen_enum_ReferrerPolicy = ["", "no-referrer", "no-referrer-when-downgrade", "origin", "origin-when-cross-origin", "unsafe-url", "same-origin", "strict-origin", "strict-origin-when-cross-origin"];


    const __wbindgen_enum_RequestCache = ["default", "no-store", "reload", "no-cache", "force-cache", "only-if-cached"];


    const __wbindgen_enum_RequestCredentials = ["omit", "same-origin", "include"];


    const __wbindgen_enum_RequestMode = ["same-origin", "no-cors", "cors", "navigate"];
    const ChangesFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_changes_free(ptr, 1));
    const ClientFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_client_free(ptr, 1));
    const IntoUnderlyingByteSourceFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_intounderlyingbytesource_free(ptr, 1));
    const IntoUnderlyingSinkFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_intounderlyingsink_free(ptr, 1));
    const IntoUnderlyingSourceFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_intounderlyingsource_free(ptr, 1));
    const JoiningFinalization = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(ptr => wasm.__wbg_joining_free(ptr, 1));

    function addToExternrefTable0(obj) {
        const idx = wasm.__externref_table_alloc();
        wasm.__wbindgen_externrefs.set(idx, obj);
        return idx;
    }

    const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(state => wasm.__wbindgen_destroy_closure(state.a, state.b));

    function debugString(val) {
        // primitive types
        const type = typeof val;
        if (type == 'number' || type == 'boolean' || val == null) {
            return  `${val}`;
        }
        if (type == 'string') {
            return `"${val}"`;
        }
        if (type == 'symbol') {
            const description = val.description;
            if (description == null) {
                return 'Symbol';
            } else {
                return `Symbol(${description})`;
            }
        }
        if (type == 'function') {
            const name = val.name;
            if (typeof name == 'string' && name.length > 0) {
                return `Function(${name})`;
            } else {
                return 'Function';
            }
        }
        // objects
        if (Array.isArray(val)) {
            const length = val.length;
            let debug = '[';
            if (length > 0) {
                debug += debugString(val[0]);
            }
            for(let i = 1; i < length; i++) {
                debug += ', ' + debugString(val[i]);
            }
            debug += ']';
            return debug;
        }
        // Test for built-in
        const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
        let className;
        if (builtInMatches && builtInMatches.length > 1) {
            className = builtInMatches[1];
        } else {
            // Failed to match the standard '[object ClassName]'
            return toString.call(val);
        }
        if (className == 'Object') {
            // we're a user defined class or Object
            // JSON.stringify avoids problems with cycles, and is generally much
            // easier than looping through ownProperties of `val`.
            try {
                return 'Object(' + JSON.stringify(val) + ')';
            } catch (_) {
                return 'Object';
            }
        }
        // errors
        if (val instanceof Error) {
            return `${val.name}: ${val.message}\n${val.stack}`;
        }
        // TODO we could test for more things here, like `Set`s and `Map`s.
        return className;
    }

    function getArrayU8FromWasm0(ptr, len) {
        ptr = ptr >>> 0;
        return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
    }

    let cachedDataViewMemory0 = null;
    function getDataViewMemory0() {
        if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
            cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
        }
        return cachedDataViewMemory0;
    }

    function getStringFromWasm0(ptr, len) {
        return decodeText(ptr >>> 0, len);
    }

    let cachedUint8ArrayMemory0 = null;
    function getUint8ArrayMemory0() {
        if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
            cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
        }
        return cachedUint8ArrayMemory0;
    }

    function handleError(f, args) {
        try {
            return f.apply(this, args);
        } catch (e) {
            const idx = addToExternrefTable0(e);
            wasm.__wbindgen_exn_store(idx);
        }
    }

    function isLikeNone(x) {
        return x === undefined || x === null;
    }

    function makeClosure(arg0, arg1, f) {
        const state = { a: arg0, b: arg1, cnt: 1 };
        const real = (...args) => {

            // First up with a closure we increment the internal reference
            // count. This ensures that the Rust closure environment won't
            // be deallocated while we're invoking it.
            state.cnt++;
            try {
                return f(state.a, state.b, ...args);
            } finally {
                real._wbg_cb_unref();
            }
        };
        real._wbg_cb_unref = () => {
            if (--state.cnt === 0) {
                wasm.__wbindgen_destroy_closure(state.a, state.b);
                state.a = 0;
                CLOSURE_DTORS.unregister(state);
            }
        };
        CLOSURE_DTORS.register(real, state, state);
        return real;
    }

    function makeMutClosure(arg0, arg1, f) {
        const state = { a: arg0, b: arg1, cnt: 1 };
        const real = (...args) => {

            // First up with a closure we increment the internal reference
            // count. This ensures that the Rust closure environment won't
            // be deallocated while we're invoking it.
            state.cnt++;
            const a = state.a;
            state.a = 0;
            try {
                return f(a, state.b, ...args);
            } finally {
                state.a = a;
                real._wbg_cb_unref();
            }
        };
        real._wbg_cb_unref = () => {
            if (--state.cnt === 0) {
                wasm.__wbindgen_destroy_closure(state.a, state.b);
                state.a = 0;
                CLOSURE_DTORS.unregister(state);
            }
        };
        CLOSURE_DTORS.register(real, state, state);
        return real;
    }

    function passArray8ToWasm0(arg, malloc) {
        const ptr = malloc(arg.length * 1, 1) >>> 0;
        getUint8ArrayMemory0().set(arg, ptr / 1);
        WASM_VECTOR_LEN = arg.length;
        return ptr;
    }

    function passArrayJsValueToWasm0(array, malloc) {
        const ptr = malloc(array.length * 4, 4) >>> 0;
        for (let i = 0; i < array.length; i++) {
            const add = addToExternrefTable0(array[i]);
            getDataViewMemory0().setUint32(ptr + 4 * i, add, true);
        }
        WASM_VECTOR_LEN = array.length;
        return ptr;
    }

    function passStringToWasm0(arg, malloc, realloc) {
        if (realloc === undefined) {
            const buf = cachedTextEncoder.encode(arg);
            const ptr = malloc(buf.length, 1) >>> 0;
            getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
            WASM_VECTOR_LEN = buf.length;
            return ptr;
        }

        let len = arg.length;
        let ptr = malloc(len, 1) >>> 0;

        const mem = getUint8ArrayMemory0();

        let offset = 0;

        for (; offset < len; offset++) {
            const code = arg.charCodeAt(offset);
            if (code > 0x7F) break;
            mem[ptr + offset] = code;
        }
        if (offset !== len) {
            if (offset !== 0) {
                arg = arg.slice(offset);
            }
            ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
            const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
            const ret = cachedTextEncoder.encodeInto(arg, view);

            offset += ret.written;
            ptr = realloc(ptr, len, offset, 1) >>> 0;
        }

        WASM_VECTOR_LEN = offset;
        return ptr;
    }

    function takeFromExternrefTable0(idx) {
        const value = wasm.__wbindgen_externrefs.get(idx);
        wasm.__externref_table_dealloc(idx);
        return value;
    }

    let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
    cachedTextDecoder.decode();
    function decodeText(ptr, len) {
        return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
    }

    const cachedTextEncoder = new TextEncoder();

    if (!('encodeInto' in cachedTextEncoder)) {
        cachedTextEncoder.encodeInto = function (arg, view) {
            const buf = cachedTextEncoder.encode(arg);
            view.set(buf);
            return {
                read: arg.length,
                written: buf.length
            };
        };
    }

    let WASM_VECTOR_LEN = 0;

    let wasmModule, wasmInstance, wasm;
    function __wbg_finalize_init(instance, module) {
        wasmInstance = instance;
        wasm = instance.exports;
        wasmModule = module;
        cachedDataViewMemory0 = null;
        cachedUint8ArrayMemory0 = null;
        wasm.__wbindgen_start();
        return wasm;
    }

    async function __wbg_load(module, imports) {
        if (typeof Response === 'function' && module instanceof Response) {
            if (!module.ok) {
                throw new Error(`failed to fetch Wasm: ${module.status} ${module.statusText} fetching '${module.url}'`);
            }

            if (typeof WebAssembly.instantiateStreaming === 'function') {
                try {
                    return await WebAssembly.instantiateStreaming(module, imports);
                } catch (e) {
                    const validResponse = expectedResponseType(module.type);

                    if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                        console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                    } else { throw e; }
                }
            }

            const bytes = await module.arrayBuffer();
            return await WebAssembly.instantiate(bytes, imports);
        } else {
            const instance = await WebAssembly.instantiate(module, imports);

            if (instance instanceof WebAssembly.Instance) {
                return { instance, module };
            } else {
                return instance;
            }
        }

        function expectedResponseType(type) {
            switch (type) {
                case 'basic': case 'cors': case 'default': return true;
            }
            return false;
        }
    }

    function initSync(module) {
        if (wasm !== undefined) return wasm;


        if (module !== undefined) {
            if (Object.getPrototypeOf(module) === Object.prototype) {
                ({module} = module)
            } else {
                console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
            }
        }

        const imports = __wbg_get_imports();
        if (!(module instanceof WebAssembly.Module)) {
            module = new WebAssembly.Module(module);
        }
        const instance = new WebAssembly.Instance(module, imports);
        return __wbg_finalize_init(instance, module);
    }

    async function __wbg_init(module_or_path) {
        if (wasm !== undefined) return wasm;


        if (module_or_path !== undefined) {
            if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
                ({module_or_path} = module_or_path)
            } else {
                console.warn('using deprecated parameters for the initialization function; pass a single object instead')
            }
        }

        if (module_or_path === undefined && script_src !== undefined) {
            module_or_path = script_src.replace(/\.js$/, "_bg.wasm");
        }
        const imports = __wbg_get_imports();

        if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
            module_or_path = fetch(module_or_path);
        }

        const { instance, module } = await __wbg_load(await module_or_path, imports);

        return __wbg_finalize_init(instance, module);
    }

    return Object.assign(__wbg_init, { initSync }, exports);
})({ __proto__: null });
