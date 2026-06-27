// Native introspection glue for the JSC backend.
//
// A few v8 inspector/console facilities have no JSC C-API equivalent because
// they read engine-internal object state: a Proxy's handler, a Promise's
// settled state/value, and the remaining contents of a Map/Set iterator
// without consuming it. Deno's `console.log`/`Deno.inspect` depend on these
// via v8's C-ABI (Proxy::GetHandler, Promise::State/Result,
// Object::PreviewEntries). We implement them against JSC's private headers,
// the same way native_modules.cpp does for the module system. Vendored-JSC
// only (the system framework ships no private headers).
#include "config.h"

#include "APICast.h"
#include "JSCInlines.h"
#include "JSCast.h"
#include "ThrowScope.h"
#include "JSGlobalObject.h"
#include "ProxyObject.h"
#include "JSPromise.h"
#include "JSMap.h"
#include "JSSet.h"
#include "JSMapIterator.h"
#include "JSSetIterator.h"
#include "JSWeakMap.h"
#include "JSWeakSet.h"
#include "WeakMapImpl.h"
#include "WeakMapImplInlines.h"
#include "ArgList.h"
#include "IterationKind.h"
#include "ArrayConstructor.h"
#include "ObjectConstructor.h"
#include <JavaScriptCore/JavaScript.h>

using namespace JSC;

extern "C" {

// Returns the handler object of a Proxy as a JSValueRef, or null if `value`
// isn't a Proxy. (Target is already reachable via the public
// JSObjectGetProxyTarget SPI.)
JSValueRef v82jsc_proxy_handler(JSContextRef ctxRef, JSValueRef value)
{
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder lock(vm);
    JSValue v = toJS(globalObject, value);
    if (auto* proxy = dynamicDowncast<ProxyObject>(v))
        return toRef(globalObject, proxy->handler());
    return nullptr;
}

// Returns a Promise's settled state (0 = pending, 1 = fulfilled, 2 = rejected,
// matching v8::Promise::PromiseState) and writes its result/reason value into
// *resultOut. Returns -1 (and leaves *resultOut untouched) if not a Promise.
int v82jsc_promise_status(JSContextRef ctxRef, JSValueRef value,
                          JSValueRef* resultOut)
{
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder lock(vm);
    JSValue v = toJS(globalObject, value);
    auto* promise = dynamicDowncast<JSPromise>(v);
    if (!promise)
        return -1;
    JSPromise::Status status = promise->status();
    if (resultOut) {
        if (status == JSPromise::Status::Pending)
            *resultOut = nullptr;
        else
            *resultOut = toRef(globalObject, promise->result());
    }
    return static_cast<int>(status);
}

// Previews the remaining entries of a Map/Set iterator without consuming the
// caller's iterator: we re-derive an independent iterator over the same backing
// collection and drain that. Sets *isKeyValueOut to true for Map `entries`
// iterators (the returned array is flattened key,value,key,value...). Returns a
// JSValueRef array, or null if `value` isn't a Map/Set iterator.
JSValueRef v82jsc_iterator_preview(JSContextRef ctxRef, JSValueRef value,
                                   bool* isKeyValueOut)
{
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder lock(vm);
    auto scope = DECLARE_THROW_SCOPE(vm);
    JSValue v = toJS(globalObject, value);

    JSArray* out = constructEmptyArray(globalObject, nullptr);
    if (!out)
        return nullptr;
    unsigned idx = 0;

    if (auto* it = dynamicDowncast<JSMapIterator>(v)) {
        bool kv = it->kind() == IterationKind::Entries;
        if (isKeyValueOut)
            *isKeyValueOut = kv;
        JSMapIterator* fresh = JSMapIterator::create(
            vm, globalObject->mapIteratorStructure(), it->iteratedObject(),
            it->kind());
        JSValue entry;
        while (fresh->next(globalObject, entry)) {
            if (scope.exception()) { (void)scope.tryClearException(); break; }
            if (kv && entry.isObject()) {
                JSValue k = entry.get(globalObject, static_cast<unsigned>(0));
                JSValue val = entry.get(globalObject, static_cast<unsigned>(1));
                (void)scope.tryClearException();
                out->putDirectIndex(globalObject, idx++, k);
                out->putDirectIndex(globalObject, idx++, val);
            } else {
                out->putDirectIndex(globalObject, idx++, entry);
            }
        }
        return toRef(globalObject, out);
    }

    if (auto* it = dynamicDowncast<JSSetIterator>(v)) {
        bool kv = it->kind() == IterationKind::Entries;
        if (isKeyValueOut)
            *isKeyValueOut = kv;
        JSSetIterator* fresh = JSSetIterator::create(
            vm, globalObject->setIteratorStructure(), it->iteratedObject(),
            it->kind());
        JSValue entry;
        while (fresh->next(globalObject, entry)) {
            if (scope.exception()) { (void)scope.tryClearException(); break; }
            if (kv && entry.isObject()) {
                JSValue k = entry.get(globalObject, static_cast<unsigned>(0));
                JSValue val = entry.get(globalObject, static_cast<unsigned>(1));
                (void)scope.tryClearException();
                out->putDirectIndex(globalObject, idx++, k);
                out->putDirectIndex(globalObject, idx++, val);
            } else {
                out->putDirectIndex(globalObject, idx++, entry);
            }
        }
        return toRef(globalObject, out);
    }

    // WeakMap / WeakSet (deno's console previews these under `showHidden`).
    // takeSnapshot() copies the live keys (WeakSet) or key,value pairs
    // (WeakMap) into a buffer without exposing the collection to script.
    if (auto* wm = dynamicDowncast<JSWeakMap>(v)) {
        if (isKeyValueOut)
            *isKeyValueOut = true;
        MarkedArgumentBuffer buffer;
        wm->takeSnapshot(buffer, 0);
        for (unsigned i = 0; i < buffer.size(); ++i)
            out->putDirectIndex(globalObject, idx++, buffer.at(i));
        (void)scope.tryClearException();
        return toRef(globalObject, out);
    }

    if (auto* ws = dynamicDowncast<JSWeakSet>(v)) {
        if (isKeyValueOut)
            *isKeyValueOut = false;
        MarkedArgumentBuffer buffer;
        ws->takeSnapshot(buffer, 0);
        for (unsigned i = 0; i < buffer.size(); ++i)
            out->putDirectIndex(globalObject, idx++, buffer.at(i));
        (void)scope.tryClearException();
        return toRef(globalObject, out);
    }

    return nullptr;
}

} // extern "C"
