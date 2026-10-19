/* Redraw from reviewed user edits. Keep a disk archive; never alter the input. */
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=__dirname,input=process.argv[2]||path.join(root,'review-20261019.json');
const source=fs.readFileSync(input,'utf8'),context={window:{}};vm.createContext(context);
for(const file of ['data.js','integration.js','implemented.js','hierarchy.js'])vm.runInContext(fs.readFileSync(path.join(root,file),'utf8'),context);
const workspace=context.window.PeriHierarchy.create(JSON.parse(source),context.window.PERI_DATA);
fs.mkdirSync(path.join(root,'archive'),{recursive:true});
const backup=path.join(root,'archive','before-redraw-20261019.json');if(!fs.existsSync(backup))fs.writeFileSync(backup,source);
fs.writeFileSync(path.join(root,'atlas-workspace.json'),JSON.stringify(workspace)+'\n');
console.log(JSON.stringify({boards:workspace.boards.length,root:workspace.ui.board,ownedModules:workspace.boards.reduce((sum,b)=>sum+b.nodes.filter(n=>!n.context&&!n.subgraph).length,0),backup}));
