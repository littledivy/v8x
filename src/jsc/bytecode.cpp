// v82jsc: JSC bytecode cache glue. Compile JS source to a
// JSC UNLINKED bytecode buffer at cache time; at startup a SourceProvider that
// returns this buffer makes JSC skip parse+codegen (CodeCache::
// getUnlinkedGlobalCodeBlock -> decodeCodeBlock). The SourceCodeKey embeds the
// JSC build hash, so a stale cache is silently ignored (never fatal).
//
// This file is the SERIALIZE half (encode). Compiled standalone by build.rs with
// the vendored WebKit private headers, same recipe as native_modules.cpp.
// config.h MUST be first.
#include "config.h"

#include "APICast.h"
#include "JSCInlines.h"
#include "CodeCache.h"
#include "CachedTypes.h"
#include "CachedBytecode.h"
#include "Completion.h"
#include "SourceCode.h"
#include "SourceProvider.h"
#include "Parser.h"
#include "JSGlobalObject.h"
#include "Identifier.h"
#include "Exception.h"
#include <wtf/MallocSpan.h>
#include <wtf/NakedPtr.h>
#include <wtf/text/WTFString.h>
#include <cstdlib>
#include <cstring>

using namespace JSC;

namespace {
// A StringSourceProvider that hands JSC a precompiled bytecode buffer, so
// CodeCache::getUnlinkedGlobalCodeBlock decodes it instead of parsing+compiling.
// JSC validates the buffer against the SourceCodeKey (version + source hash) and
// silently falls back to parsing on mismatch, so a stale cache is never fatal.
class CachedSourceProvider final : public StringSourceProvider {

public:
    static Ref<CachedSourceProvider> create(const String& source,
        const SourceOrigin& origin, String url, SourceTaintedOrigin taint,
        SourceProviderSourceType type, RefPtr<CachedBytecode>&& bc)
    {
        return adoptRef(*new CachedSourceProvider(source, origin,
            std::move(url), taint, type, std::move(bc)));
    }
    RefPtr<CachedBytecode> cachedBytecode() const final { return m_cached; }
private:
    CachedSourceProvider(const String& source, const SourceOrigin& origin,
        String&& url, SourceTaintedOrigin taint, SourceProviderSourceType type,
        RefPtr<CachedBytecode>&& bc)
        : StringSourceProvider(source, origin, taint, std::move(url),
            TextPosition(), type)
        , m_cached(std::move(bc))
    {
    }
    RefPtr<CachedBytecode> m_cached;
};
} // namespace

extern "C" {

// Compile `src` (ES module if is_module, else Program/CJS) with key `url` to a
// JSC unlinked-bytecode buffer. Returns a malloc'd buffer (free with
// v82jsc_bytecode_free) and sets *outLen, or null on parse/encode error.
uint8_t* v82jsc_bytecode_encode(JSContextRef ctxRef, const char* url,
                                const char* src, int is_module, size_t* outLen)
{
    if (outLen)
        *outLen = 0;
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);

    String urlStr = String::fromUTF8(url);
    SourceOrigin sourceOrigin { URL { urlStr } };
    SourceCode sourceCode = makeSource(String::fromUTF8(src), sourceOrigin,
        SourceTaintedOrigin::Untainted, urlStr);

    ParserError error;
    RefPtr<CachedBytecode> cached;
    if (is_module) {
        UnlinkedModuleProgramCodeBlock* ucb =
            recursivelyGenerateUnlinkedCodeBlockForModuleProgram(vm, sourceCode,
                StrictModeLexicallyScopedFeature, JSParserScriptMode::Module,
                { }, error, EvalContextType::None);
        if (error.isValid() || !ucb)
            return nullptr;
        SourceCodeKey key = sourceCodeKeyForSerializedModule(vm, sourceCode);
        cached = encodeCodeBlock(vm, key, ucb);
    } else {
        UnlinkedProgramCodeBlock* ucb =
            recursivelyGenerateUnlinkedCodeBlockForProgram(vm, sourceCode,
                NoLexicallyScopedFeatures, JSParserScriptMode::Classic,
                { }, error, EvalContextType::None);
        if (error.isValid() || !ucb)
            return nullptr;
        SourceCodeKey key = sourceCodeKeyForSerializedProgram(vm, sourceCode);
        cached = encodeCodeBlock(vm, key, ucb);
    }
    if (!cached)
        return nullptr;

    std::span<const uint8_t> bytes = cached->span();
    uint8_t* out = static_cast<uint8_t*>(malloc(bytes.size()));
    if (!out)
        return nullptr;
    memcpy(out, bytes.data(), bytes.size());
    if (outLen)
        *outLen = bytes.size();
    return out;
}

void v82jsc_bytecode_free(uint8_t* p)
{
    free(p);
}

// Evaluate `src` as a Program, using `bytecode` (if non-null) as its precompiled
// unlinked bytecode so JSC skips parse+codegen. Returns the result JSValueRef,
// or null on error (with *excOut set). A stale/invalid buffer is ignored by JSC
// (it falls back to parsing), so this is always correct, just maybe not cached.
JSValueRef v82jsc_program_eval_cached(JSContextRef ctxRef, const char* url,
    const char* src, const uint8_t* bytecode, size_t bytecodeLen,
    JSValueRef* excOut)
{
    JSGlobalObject* globalObject = toJS(ctxRef);
    VM& vm = globalObject->vm();
    JSLockHolder locker(vm);

    RefPtr<CachedBytecode> cached;
    if (bytecode && bytecodeLen) {
        auto buf = MallocSpan<uint8_t, VMMalloc>::malloc(bytecodeLen);
        memcpy(buf.mutableSpan().data(), bytecode, bytecodeLen);
        cached = CachedBytecode::create(std::move(buf), { });
    }

    String urlStr = String::fromUTF8(url);
    SourceOrigin sourceOrigin { URL { urlStr } };
    Ref<CachedSourceProvider> provider = CachedSourceProvider::create(
        String::fromUTF8(src), sourceOrigin, urlStr,
        SourceTaintedOrigin::Untainted, SourceProviderSourceType::Program,
        std::move(cached));
    SourceCode sourceCode(std::move(provider));

    NakedPtr<Exception> exception;
    JSValue result = JSC::evaluate(globalObject, sourceCode, JSValue(), exception);
    if (exception) {
        if (excOut)
            *excOut = toRef(globalObject, exception->value());
        return nullptr;
    }
    return toRef(globalObject, result);
}

} // extern "C"
