import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import {spawn} from "node:child_process";
const root = process.cwd();
const source = path.join(root,".build/target/debug/examples/bk-cat-native-probe.exe");
if (!fs.existsSync(source)) throw new Error("Built executable is missing");
for(const key of ["CONCAT_GPU","SLINT_BACKEND","SLINT_RENDERER"]) if(process.env[key]!==undefined) throw new Error("Unexpected rendering override: "+key);
const env={...process.env};
const pathKey=Object.keys(env).find(key=>key.toLowerCase()==="path")??"PATH";
env[pathKey]=[
path.join(root,".build/native/ffmpeg-win64/ffmpeg-n8.1.3-9-g29e619e767-win64-lgpl-shared-8.1/bin"),
path.join(root,".build/native/onnxruntime-win-x64/onnxruntime-win-x64-1.28.0/lib"),
path.join(root,".build/target/debug"),env[pathKey]??""].join(path.delimiter);
const previous=path.join(root,".build/m3-native-probe-runs.json");
if(fs.existsSync(previous)&&!fs.existsSync(path.join(root,".build/m3-native-probe-runs-initial.json"))) fs.copyFileSync(previous,path.join(root,".build/m3-native-probe-runs-initial.json"),fs.constants.COPYFILE_EXCL);
const runRoot=path.join(root,".build/target/bk-cat-probes",crypto.randomUUID().replaceAll("-",""));
fs.mkdirSync(runRoot,{recursive:true});
const results=[];
for(const mode of ["native","platform"]) {
 const directory=path.join(runRoot,mode); fs.mkdirSync(directory); fs.mkdirSync(path.join(directory,"portable"));
 const exe=path.join(directory,"bk-cat-native-probe.exe"); fs.copyFileSync(source,exe,fs.constants.COPYFILE_EXCL);
 for(const file of fs.readdirSync(path.join(root,".build/target/debug"))) if(file.endsWith(".dll")) fs.copyFileSync(path.join(root,".build/target/debug",file),path.join(directory,file),fs.constants.COPYFILE_EXCL);
 const stdout=fs.openSync(path.join(directory,"stdout.log"),"wx");
 const stderr=fs.openSync(path.join(directory,"stderr.log"),"wx");
 let timedOut=false;
 const ended=await new Promise((resolve,reject)=>{
   const child=spawn(exe,["--mode",mode],{cwd:directory,env,stdio:["ignore",stdout,stderr]});
   const timer=setTimeout(()=>{timedOut=true;child.kill();},180000);
   child.once("error",error=>{clearTimeout(timer);reject(error);});
   child.once("close",(code,signal)=>{clearTimeout(timer);resolve({code,signal});});
 });
 fs.closeSync(stdout); fs.closeSync(stderr);
 const reportPath=path.join(directory,"probe-report.json");
 const report=fs.existsSync(reportPath)?JSON.parse(fs.readFileSync(reportPath,"utf8")):null;
 const result={mode,directory,executable_sha256:crypto.createHash("sha256").update(fs.readFileSync(exe)).digest("hex"),exit_code:ended.code,signal:ended.signal,timed_out:timedOut,report_present:report!==null,passed:ended.code===0&&!timedOut&&report?.passed===true};
 results.push(result);fs.writeFileSync(previous,JSON.stringify(results,null,2)+"\n","utf8");
 console.log(JSON.stringify(result));
 console.log(JSON.stringify({mode,steps:report?.steps?.map(step=>step.name),error:report?.error,rendering:report?.rendering}));
}
process.exitCode=results.every(result=>result.passed)?0:1;
