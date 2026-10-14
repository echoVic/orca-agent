// Test-only owned copy: redirect all application storage before its original main entry loads.
const fs=require('fs'),path=require('path'),child=require('child_process'),crypto=require('crypto');
const asar=require(process.env.ORCA_CU_ASAR_MODULE || '@electron/asar');
(async()=>{
 const source=process.argv[2], bundle=process.argv[3], content=path.join(bundle,'Contents');
 if (!source || !bundle || !path.resolve(bundle).startsWith('/tmp/')) throw new Error('Usage: node isolate-electron-profile.cjs <installed .app> </tmp/owned .app>');
 const extracted=bundle+'.source', profile=bundle+'.profile';
 fs.mkdirSync(content,{recursive:true});fs.mkdirSync(path.join(content,'MacOS'),{recursive:true});fs.mkdirSync(path.join(content,'Resources'),{recursive:true});
 const plistScript=`import plistlib,json; p=plistlib.load(open(${JSON.stringify(source+'/Contents/Info.plist')},'rb')); print(json.dumps(p))`;
 const plist=JSON.parse(child.execFileSync('python3',['-c',plistScript],{encoding:'utf8'}));
 const exe=plist.CFBundleExecutable;
 fs.copyFileSync(path.join(source,'Contents','MacOS',exe),path.join(content,'MacOS',exe)); fs.chmodSync(path.join(content,'MacOS',exe),0o755);
 if(!fs.existsSync(path.join(content,'Frameworks')))fs.cpSync(path.join(source,'Contents','Frameworks'),path.join(content,'Frameworks'),{recursive:true,verbatimSymlinks:true});
 if(!fs.existsSync(extracted)){fs.mkdirSync(extracted);asar.extractAll(path.join(source,'Contents','Resources','app.asar'),extracted);}
 const main=path.join(extracted,'out','main','index.js');
 let code=fs.readFileSync(main,'utf8');
 if(!code.startsWith('// ORCA_OWNED_PROFILE')) {
 const prefix=`// ORCA_OWNED_PROFILE\nconst __cuFs=require('node:fs'),__cuOs=require('node:os'),__cuPath=require('node:path'),__cuApp=require('electron').app;\nconst __cuRoot=${JSON.stringify(profile)}; __cuFs.mkdirSync(__cuRoot,{recursive:true}); __cuOs.homedir=()=>__cuPath.join(__cuRoot,'home');\nfor(const key of ['home','appData','userData','sessionData','logs','documents','downloads','desktop','temp']){const dir=__cuPath.join(__cuRoot,key);__cuFs.mkdirSync(dir,{recursive:true});__cuApp.setPath(key,dir);}\n`;
 fs.writeFileSync(main,prefix+code);
 }
 const archive=path.join(content,'Resources','app.asar');
 await asar.createPackageWithOptions(extracted,archive,{unpack:'*.node'});
 for(const entry of fs.readdirSync(path.join(source,'Contents','Resources'))) {
   if(entry.startsWith('app.asar'))continue;
   const target=path.join(content,'Resources',entry);if(!fs.existsSync(target))fs.symlinkSync(path.join(source,'Contents','Resources',entry),target);
 }
 const header=asar.getRawHeader(archive).headerString;
 plist.ElectronAsarIntegrity={'Resources/app.asar':{algorithm:'SHA256',hash:crypto.createHash('sha256').update(header).digest('hex')}};
 fs.writeFileSync(bundle+'.info.json',JSON.stringify(plist));
 child.execFileSync('python3',['-c',`import json,plistlib; p=json.load(open(${JSON.stringify(bundle+'.info.json')},'r')); plistlib.dump(p,open(${JSON.stringify(path.join(content,'Info.plist'))},'wb'))`]);
 child.execFileSync('codesign',['--force','--deep','--sign','-','--preserve-metadata=entitlements,flags',bundle]);
 console.log(JSON.stringify({bundle,profile,canonical:plist.CFBundleIdentifier,source,allAppHomeAndStoragePaths:'owned /tmp paths; HOME and TMPDIR unchanged'}));
})().catch(e=>{console.error(e);process.exitCode=1});
