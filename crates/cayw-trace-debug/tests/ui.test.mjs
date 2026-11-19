import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
const source=readFileSync(new URL('../assets/app.js', import.meta.url),'utf8');
class E {
 constructor(){this.children=[];this.dataset={};this.value='';this.className='';}
 append(...a){this.children.push(...a)} replaceChildren(...a){this.children=a}
 setAttribute(k,v){this[k]=v;if(k==='class')this.className=v} addEventListener(){} focus(){}
 get classList(){return {contains:n=>this.className.split(' ').includes(n)}}
}
function harness(){
 const context=vm.createContext({document:{activeElement:null,createElement:()=>new E,createElementNS:()=>new E},Node:E,setTimeout,clearTimeout,AbortController,console});
 vm.runInContext(source.replace('  boot();', `
 globalThis.audit={layoutModules,preferredNodeForModule,renderNodeGraph,undoToVersion,
 setup(d,p,e,u){doc=d;selectedProject=p;selectedEvaluation=e;undoTarget=u;ui=new Proxy({}, {get:(o,k)=>o[k]||(o[k]=new Node)});},
 filter(v){moduleFilter=v;}, nodes(){return Array.from(nodeElements.values())},
 switchProject(p,e){selectedProject=p;selectedEvaluation=e;},
 selected(){return {project:selectedProject,evaluation:selectedEvaluation}},
 stubTransport(fn){api=fn;renderAll=()=>{};updateActionButtons=()=>{};renderHistory=()=>{};},};
 `),context);
 return context.audit;
}
test('unnamed module retains its raw key and visible node',()=>{
 const a=harness();a.setup(null,null,null,null);
 const plan={nodes:[{id:'n',module:'',label:'N'}],edges:[]};
 const label=a.layoutModules(plan.nodes).order[0];
 assert.equal(label,'');
 assert.equal(a.preferredNodeForModule(plan,null,label,null).id,'n');
 a.filter(label);a.renderNodeGraph(plan,null,null);
 assert.doesNotMatch(a.nodes()[0].className,/is-hidden/);
});
test('late undo response preserves another project selected evaluation',async()=>{
 const a=harness();
 const project=(id,versions)=>({id,revisions:versions.map(version=>({version,plan:{nodes:[],edges:[]}})),evaluations:versions.map(version=>({version,steps:[]}))});
 const d={schema:'cayw.trace.state.v1',sequence:1,reset:true,projects:{A:project('A',[1,2]),B:project('B',[1,3])}};
 a.setup(d,'A',2,1);
 let resolve;a.stubTransport(()=>new Promise(r=>resolve=r));
 const pending=a.undoToVersion();
 a.switchProject('B',1);
 resolve({status:200,payload:{...d,sequence:2,projects:{...d.projects,A:project('A',[1,2,3])}}});
 await pending;
 assert.equal(a.selected().project,'B');
 assert.equal(a.selected().evaluation,1);
});

test('empty and literally named unnamed modules remain separate',()=>{
 const a=harness();a.setup(null,null,null,null);
 const nodes=[{id:'empty',module:''},{id:'named',module:'未命名模块'}];
 assert.equal(a.layoutModules(nodes).groups.size,2);
 assert.equal(a.preferredNodeForModule({nodes},null,'',null).id,'empty');
 assert.equal(a.preferredNodeForModule({nodes},null,'未命名模块',null).id,'named');
});
