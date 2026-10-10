import json,glob,statistics as st
L=lambda p:[json.loads(l) for f in glob.glob(p) for l in open(f)]
v1=[r for r in L("eval/archive-v1/results/*/results.jsonl") if r["system"].startswith("hx")]
v2=L("eval/archive-v2/results-v2/*/results.jsonl"); v3=L("eval/results-v3/*/results.jsonl")
cl=[r for r in L("eval/results/*/results.jsonl") if r["system"]=="claude"]
T=["shop-feature-bogo","taskq-feature-deps","shop-refactor-cents","wiki-refactor-blocks"]
med=lambda x:st.median(x) if x else None
def row(rs,t):
    s=[r for r in rs if r["task"]==t]; ok=[r for r in s if r["passed"]]
    ct=sum(r.get("code_turns",0) for r in s); er=sum(r.get("luau_errors",0) for r in s)
    return dict(p=f"{len(ok)}/{len(s)}",tok=med([r["tokens_total"] for r in ok]),fr=med([r["tokens_fresh"] for r in ok]),turns=med([r["turns"] for r in s]),cost=med([r["cost_usd"] for r in ok if r.get("cost_usd")]),err=f"{er}/{ct}")
f=lambda x,k=0:"-" if x is None else f"{x:.{k}f}"
print(f"{'task':<22}{'sys':<8}{'pass':<5}{'tok':>8}{'fresh':>8}{'turns':>6}{'luauErr':>8}{'cost':>8}"); R={}
for t in T:
    for n,rs in (("hx-v1",v1),("hx-v2",v2),("hx-v3",v3),("claude",cl)):
        d=row(rs,t);R[(t,n)]=d
        print(f"{t:<22}{n:<8}{d['p']:<5}{f(d['tok']):>8}{f(d['fr']):>8}{f(d['turns'],1):>6}{d['err']:>8}{f(d['cost'],4):>8}")
for n in ("hx-v1","hx-v2","hx-v3"):
    out=[]
    for k in ("tok","fr","cost"):
        rs=[R[(t,n)][k]/R[(t,"claude")][k] for t in T if R[(t,n)][k] and R[(t,"claude")][k]]
        out.append(f"{k}={med(rs):.2f}(n={len(rs)})" if rs else f"{k}=-")
    print(n,"ratio vs claude:"," ".join(out))
for n,rs in (("v2",v2),("v3",v3)):
    ct=sum(r["code_turns"] for r in rs); er=sum(r["luau_errors"] for r in rs)
    print(n,"luau errors",er,"/",ct,f"= {100*er/ct:.1f}%","pass",sum(r["passed"] for r in rs),"/",len(rs),"trunc",sum(r.get("truncated_turns",0) for r in rs))
