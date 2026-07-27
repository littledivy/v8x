#include "quickjs.h"
#include <stdio.h>
#include <string.h>
#include <time.h>
#define WR (1|(1<<3))
#define RD (1|(1<<3))
extern void v82jsc_snapshot_capture_intrinsics(JSContext*,JSValue);
int v82jsc_snapshot_write_host_object(JSContext*c,JSValueConst o,uint8_t*b,size_t*s){return 0;}
int v82jsc_snapshot_read_host_object(JSContext*c,const uint8_t*b,size_t s,JSValue*o){return 0;}
bool v82jsc_snapshot_host_object_has_prototype(JSValueConst o){return false;}
static double ms(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}
static void ri(JSContext*ctx){JSValue g=JS_GetGlobalObject(ctx);JSValue r=JS_GetPropertyStr(ctx,g,"__v8x_snapshot_intrinsics");if(!JS_IsObject(r)){JS_FreeValue(ctx,r);r=JS_NewArray(ctx);JS_DefinePropertyValueStr(ctx,g,"__v8x_snapshot_intrinsics",JS_DupValue(ctx,r),JS_PROP_CONFIGURABLE|JS_PROP_WRITABLE);}v82jsc_snapshot_capture_intrinsics(ctx,r);JS_FreeValue(ctx,r);JS_FreeValue(ctx,g);}
// Expensive bootstrap: build N objects with computed fields + a big Map.
static const char*BOOT=
"globalThis.data=(function(){const N=20000;const arr=[];"
"for(let i=0;i<N;i++){arr.push({id:i,sq:i*i,name:'item'+i,tags:[i%3,i%5,i%7]});}"
"const idx=new Map();for(const o of arr)idx.set(o.name,o);"
"return{arr,idx,count:N};})();";
int main(void){
  JSRuntime*rt1=JS_NewRuntime();JSContext*c1=JS_NewContext(rt1);
  double t=ms();JSValue r=JS_Eval(c1,BOOT,strlen(BOOT),"<b>",JS_EVAL_TYPE_GLOBAL);double t_boot=ms()-t;JS_FreeValue(c1,r);
  ri(c1);JSValue g=JS_GetGlobalObject(c1);size_t bs=0;
  t=ms();uint8_t*blob=JS_WriteObject(c1,&bs,g,WR);double t_w=ms()-t;JS_FreeValue(c1,g);
  if(!blob){printf("write fail\n");return 1;}
  JSRuntime*rt2=JS_NewRuntime();JSContext*c2=JS_NewContext(rt2);ri(c2);
  t=ms();JSValue rv=JS_ReadObject(c2,blob,bs,RD);double t_r=ms()-t;
  int ok=!JS_IsException(rv);
  // verify a couple fields
  JSValue g2=JS_GetGlobalObject(c2);JSValue d=JS_GetPropertyStr(c2,rv,"data");JS_SetPropertyStr(c2,g2,"data",d);JS_FreeValue(c2,g2);
  JSValue chk=JS_Eval(c2,"data.arr[12345].sq===12345*12345 && data.idx.get('item9999').id===9999 && data.count===20000",64,"<c>",JS_EVAL_TYPE_GLOBAL);
  int verified=JS_ToBool(c2,chk);JS_FreeValue(c2,chk);
  printf("SCALE (N=20000 objects):\n");
  printf("  from-source bootstrap eval: %.3f ms\n",t_boot);
  printf("  snapshot write:             %.3f ms, blob %zu bytes (%.1f KB)\n",t_w,bs,bs/1024.0);
  printf("  snapshot restore:           %.3f ms  [%.1fx faster than re-boot]\n",t_r,t_boot/t_r);
  printf("  restore ok=%d verified=%d\n",ok,verified);
  js_free(c1,blob);return (ok&&verified)?0:2;
}
