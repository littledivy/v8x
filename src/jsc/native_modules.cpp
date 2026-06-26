// v82jsc: native JSC ES-module glue. Replaces the rewrite_es_module string
// rewriter with real JSModuleRecords. Compiled standalone by build.rs with the
// vendored WebKit private headers (see native-modules-plan). config.h MUST be
// first (it #undef's the new/delete error-guard the JSC prefix installs).
#include "config.h"

#include "APICast.h"
#include "JSCInlines.h"
#include "JSModuleRecord.h"
#include "CyclicModuleRecord.h"
#include "AbstractModuleRecord.h"
#include "JSPromise.h"
#include "JSModuleNamespaceObject.h"
#include "SyntheticModuleRecord.h"
#include "JSModuleEnvironment.h"
#include "JSSymbolTableObject.h"
#include "MarkedVector.h"
#include "ModuleAnalyzer.h"
#include "ModuleMap.h"
#include "ScriptFetchParameters.h"
#include "Nodes.h"
#include "Parser.h"
#include "JSGlobalObject.h"
#include "Identifier.h"
#include "SourceCode.h"
#include "Completion.h"
#include "ThrowScope.h"
#include "StrongInlines.h"
#include <wtf/text/WTFString.h>
#include <cstdio>

#define NMLOG(...) do { if (getenv("V82JSC_NM_DEBUG")) { fprintf(stderr, "[nm] " __VA_ARGS__); fputc('\n', stderr); fflush(stderr); } } while(0)

using namespace JSC;

extern "C" {

// Parse `src` as an ES module with key `url`. On success returns a GC-protected
// JSModuleRecord* (opaque void*); the caller owns it and must release it with
// v82jsc_module_release. On parse/analyze error returns null and, if exceptionOut
// is non-null, stores the error value there.
void* v82jsc_module_parse(JSContextRef ctxRef, const char* url, const char* src,
                          JSValueRef* exceptionOut)
{
    NMLOG("parse enter url=%s", url ? url : "(null)");
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    auto scope = DECLARE_THROW_SCOPE(vm);

    String urlStr = String::fromUTF8(url);
    Identifier moduleKey = Identifier::fromString(vm, urlStr);
    SourceOrigin sourceOrigin { URL { urlStr } };
    SourceCode sourceCode = makeSource(String::fromUTF8(src), sourceOrigin,
        SourceTaintedOrigin::Untainted, urlStr);

    ParserError error;
    std::unique_ptr<ModuleProgramNode> node = parseRootNode<ModuleProgramNode>(
        vm, sourceCode, ImplementationVisibility::Public,
        JSParserBuiltinMode::NotBuiltin, StrictModeLexicallyScopedFeature,
        JSParserScriptMode::Module, SourceParseMode::ModuleAnalyzeMode, error);
    if (error.isValid() || !node) {
        if (exceptionOut)
            *exceptionOut = toRef(globalObject,
                error.toErrorObject(globalObject, sourceCode));
        return nullptr;
    }

    ModuleAnalyzer analyzer(globalObject, moduleKey, sourceCode,
        node->varDeclarations(), node->lexicalVariables(), node->features());
    auto result = analyzer.analyze(*node);
    if (!result) {
        auto [errorType, message] = std::move(result.error());
        if (exceptionOut)
            *exceptionOut = toRef(globalObject,
                createError(globalObject, errorType, message));
        return nullptr;
    }
    NMLOG("parse ok");
    JSModuleRecord* record = result.value();
    // Keep it alive across the deno CompileModule -> Instantiate -> Evaluate
    // round trip via a heap-rooted Strong handle.
    auto* handle = new Strong<AbstractModuleRecord>(vm, record);
    return handle;
}

// Number of `import`/`export ... from` requests (deno's GetModuleRequests).
int v82jsc_module_request_count(void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    return static_cast<int>(h->get()->requestedModules().size());
}

// UTF-8 of the i-th request specifier into buf (cap bytes); returns byte length
// (excluding NUL) or -1 if out of range.
int v82jsc_module_request_at(void* handle, int i, char* buf, int cap)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    const auto& reqs = h->get()->requestedModules();
    if (i < 0 || static_cast<unsigned>(i) >= reqs.size())
        return -1;
    String spec = reqs[i].m_specifier.string();
    CString utf8 = spec.utf8();
    int n = static_cast<int>(utf8.length());
    if (cap > 0) {
        int copy = n < cap - 1 ? n : cap - 1;
        memcpy(buf, utf8.data(), copy);
        buf[copy] = '\0';
    }
    return n;
}

// Import-attribute type of the i-th request: 0 None, 1 JavaScript, 2
// WebAssembly, 3 JSON (deno reads this as the `with { type: ... }` attribute).
int v82jsc_module_request_attr_type(void* handle, int i)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    const auto& reqs = h->get()->requestedModules();
    if (i < 0 || static_cast<unsigned>(i) >= reqs.size())
        return 0;
    auto attrs = reqs[i].m_attributes;
    if (!attrs)
        return 0;
    return static_cast<int>(attrs->type());
}

// Register `dep` as the module resolved for import `specifier` in `parent`'s
// loaded-modules map, so parent->link() resolves the edge with no module-loader
// fetch (mirrors what JSModuleLoader::notifyCompletion does during a real load).
// deno drives the graph: it calls this for every (parent, specifier) -> dep edge
// before link. Returns false if `specifier` isn't one of parent's requests.
bool v82jsc_module_add_dependency(JSContextRef ctxRef, void* parentHandle,
                                  const char* specifier, void* depHandle)
{
    auto* ph = static_cast<Strong<AbstractModuleRecord>*>(parentHandle);
    auto* dh = static_cast<Strong<AbstractModuleRecord>*>(depHandle);
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    AbstractModuleRecord* parent = ph->get();
    AbstractModuleRecord* dep = dh->get();
    String spec = String::fromUTF8(specifier);
    for (const auto& req : parent->requestedModules()) {
        if (req.m_specifier.string() == spec) {
            ModuleMapKey key { req.m_specifier.impl(), req.type() };
            AbstractModuleRecord::LoadedModuleRequest value { vm, req, dep, parent };
            parent->loadedModules().add(std::move(key), std::move(value));
            return true;
        }
    }
    return false;
}

// The record's link/evaluate status, mapped to deno's ModuleStatus ordinals:
// 0 Uninstantiated, 1 Instantiating, 2 Instantiated, 3 Evaluating,
// 4 Evaluated, 5 Errored. Returns -1 for a non-cyclic (synthetic) record so the
// caller keeps using its own wrapper status. Lets deno see a dep evaluated by
// JSC's graph cascade (not via a per-module deno Evaluate call).
int v82jsc_module_status(void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    auto* cyclic = dynamicDowncast<CyclicModuleRecord>(h->get());
    if (!cyclic)
        return -1;
    switch (cyclic->status()) {
    case CyclicModuleRecord::Status::New:
    case CyclicModuleRecord::Status::Unlinked:
        return 0;
    case CyclicModuleRecord::Status::Linking:
        return 1;
    case CyclicModuleRecord::Status::Linked:
        return 2;
    case CyclicModuleRecord::Status::Evaluating:
        return 3;
    // A top-level-await module is EvaluatingAsync after evaluate() returns (its
    // promise tracks TLA completion). V8 reports such a module as Evaluated, and
    // deno asserts Evaluated/Errored post-mod_evaluate — so map it to Evaluated.
    case CyclicModuleRecord::Status::EvaluatingAsync:
    case CyclicModuleRecord::Status::Evaluated:
        return 4;
    }
    return -1;
}

// Link the (already dependency-injected) record. Returns false if it threw.
bool v82jsc_module_link(JSContextRef ctxRef, void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    auto scope = DECLARE_THROW_SCOPE(vm);
    NMLOG("link enter");
    h->get()->link(globalObject, nullptr);
    NMLOG("link done exc=%d", scope.exception()?1:0);
    return !scope.exception();
}

// Evaluate the linked module. Returns the evaluation result (a promise for
// async module graphs) as a JSValueRef.
JSValueRef v82jsc_module_evaluate(JSContextRef ctxRef, void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    // CyclicModuleRecord::evaluate drives the WHOLE linked graph (deps first,
    // cycles handled) and returns the top-level-await promise. NOT the 3-arg
    // JSModuleRecord::evaluate, which only steps this module's body coroutine.
    NMLOG("evaluate enter");
    CyclicModuleRecord* cyclic = dynamicDowncast<CyclicModuleRecord>(h->get());
    if (!cyclic)
        return toRef(globalObject, jsUndefined());
    JSPromise* promise = cyclic->evaluate(globalObject);
    NMLOG("evaluate done promise=%p", (void*)promise);
    return toRef(globalObject, JSValue(promise));
}

// The module's namespace object (deno's GetModuleNamespace).
JSValueRef v82jsc_module_namespace(JSContextRef ctxRef, void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    JSModuleNamespaceObject* ns = h->get()->getModuleNamespace(globalObject);
    return toRef(globalObject, JSValue(ns));
}

void v82jsc_module_release(void* handle)
{
    delete static_cast<Strong<AbstractModuleRecord>*>(handle);
}

// Raw AbstractModuleRecord* behind a handle — used as the key for the Rust-side
// record->Module map so the import.meta hook can find deno's module wrapper.
void* v82jsc_module_record_ptr(void* handle)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    return static_cast<void*>(h->get());
}

// --- Synthetic-module bridge: back deno's V8-synthetic modules (the ops module,
// JSON, node: facades) with a real JSC SyntheticModuleRecord so native ESM
// records can link to them. deno declares export NAMES at create and fills
// VALUES later (SetSyntheticModuleExport at evaluate); we create with undefined
// placeholders and update the module environment in place (live bindings). ---

// Create a synthetic record with `count` export names (values undefined).
// Returns an opaque Strong<AbstractModuleRecord>* handle, null on failure.
void* v82jsc_synthetic_create(JSContextRef ctxRef, const char* url,
                              const char* const* names, int count)
{
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    auto scope = DECLARE_THROW_SCOPE(vm);

    Identifier moduleKey = Identifier::fromString(vm, String::fromUTF8(url));
    Vector<Identifier, 4> exportNames;
    MarkedArgumentBuffer exportValues;
    for (int i = 0; i < count; ++i) {
        exportNames.append(Identifier::fromString(vm, String::fromUTF8(names[i])));
        exportValues.append(jsUndefined());
    }
    NMLOG("synthetic_create url=%s n=%d", url ? url : "(null)", count);
    SyntheticModuleRecord* record =
        SyntheticModuleRecord::tryCreateWithExportNamesAndValues(
            globalObject, moduleKey, exportNames, exportValues);
    if (!record)
        return nullptr;
    return new Strong<AbstractModuleRecord>(vm, record);
}

// Set/replace an export value in a synthetic record's module environment.
// Works post-link (live binding) via the symbol-table watchpoint put.
bool v82jsc_synthetic_set_export(JSContextRef ctxRef, void* handle,
                                 const char* name, JSValueRef value)
{
    auto* h = static_cast<Strong<AbstractModuleRecord>*>(handle);
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);
    JSModuleEnvironment* env = h->get()->moduleEnvironmentMayBeNull();
    if (!env)
        return false;
    Identifier id = Identifier::fromString(vm, String::fromUTF8(name));
    JSValue v = toJS(globalObject, value);
    bool putResult = false;
    symbolTablePutTouchWatchpointSet(env, globalObject, id, v,
        /*shouldThrowReadOnlyError*/ false, /*ignoreReadOnlyErrors*/ true,
        putResult);
    return putResult;
}

} // extern "C"
