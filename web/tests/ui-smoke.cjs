const { chromium } = require(process.env.POOLSYNC_PLAYWRIGHT_MODULE || 'playwright');
const assert = require('node:assert/strict');const fs=require('node:fs');const path=require('node:path');
const artifacts=process.env.POOLSYNC_UI_ARTIFACTS || '/tmp/poolsync-ui-review';
(async () => {
 const browser=await chromium.launch({executablePath:process.env.POOLSYNC_CHROME || undefined,headless:true,args:['--no-sandbox']});
 const page=await browser.newPage({viewport:{width:1366,height:900}});const errors=[];page.on('pageerror',e=>errors.push(e.message));
 const node=(x,y,w,h,k=true)=>({x,y,width:w,height:h,kvm_enabled:k,neighbors:{},monitor_x:0,monitor_y:0,desktop_x:0,desktop_y:0,desktop_width:w,desktop_height:h});
 let topology={nodes:{laptop:node(-1366,-200,1366,768),desk:node(0,0,2560,1440),clip:node(0,100000,1920,1080,false)}};let saved=null;
 let status={hub:{node_count:2},master:'desk',nodes:Object.entries(topology.nodes).map(([name,n])=>({name,online:name!=='clip',mode:n.kvm_enabled?'full':'clipboard_only',screen:{width:n.width,height:n.height},kvm_enabled:n.kvm_enabled,clipboard_sync:true,local_active:true,is_master:name==='desk',monitors:[]}))};
 await page.addInitScript(()=>localStorage.setItem('poolsync_token','test-only'));
 await page.route('**/api/**', async route=>{
   const path=new URL(route.request().url()).pathname;
   if(path==='/api/topology'&&route.request().method()==='POST'){saved=route.request().postDataJSON();topology=saved;return route.fulfill({status:200,body:''})}
   await route.fulfill({contentType:'application/json',body:JSON.stringify(path==='/api/status'?status:topology)});
 });
 await page.goto(process.env.POOLSYNC_TEST_URL || 'http://127.0.0.1:9476/');await page.getByRole('button',{name:'Config KVM',exact:true}).click();
 const laptop=page.getByRole('button',{name:'Déplacer Laptop',exact:true});await laptop.waitFor();
 assert.equal(await page.getByRole('button',{name:/^Déplacer /}).count(),2);
 const initial=await laptop.boundingBox();await laptop.focus();await page.keyboard.press('ArrowLeft');
 await page.getByRole('status').waitFor();assert((await laptop.boundingBox()).x>=0);
 await page.getByRole('button',{name:'Annuler',exact:true}).click();await page.getByRole('button',{name:'Refaire',exact:true}).click();
 await page.getByRole('button',{name:'Enregistrer',exact:true}).click();await page.getByText('Topologie envoyée aux agents').waitFor();
 assert.equal(saved.nodes.laptop.x,-1386); // grid step preserves snapped keyboard movement
 const beforeDrag=saved.nodes.laptop.x;const b=await laptop.boundingBox();
 await page.mouse.move(b.x+b.width/2,b.y+b.height/2);await page.mouse.down();await page.mouse.move(b.x+b.width/2+90,b.y+b.height/2,{steps:8});await page.mouse.up();
 await page.getByRole('button',{name:'Enregistrer',exact:true}).click();await page.getByText('Topologie envoyée aux agents').waitFor();assert.notEqual(saved.nodes.laptop.x,beforeDrag);
 await page.getByRole('button',{name:'Annuler',exact:true}).click();await page.getByRole('button',{name:'Enregistrer',exact:true}).click();await page.getByText('Topologie envoyée aux agents').waitFor();assert.equal(saved.nodes.laptop.x,beforeDrag);
 fs.mkdirSync(artifacts,{recursive:true});await page.screenshot({path:path.join(artifacts,'desktop.png'),fullPage:true});
 await page.setViewportSize({width:390,height:844});await page.screenshot({path:path.join(artifacts,'narrow.png'),fullPage:true});
 assert.equal(await page.locator('body').evaluate(e=>e.scrollWidth>window.innerWidth),false);
 assert.deepEqual(errors,[]);fs.writeFileSync(path.join(artifacts,'result.json'),JSON.stringify({passed:true,checks:['mixed resolutions','negative positions','keyboard nudge','undo/redo','drag undo','save','narrow viewport','no runtime errors']},null,2));
 console.log('UI_SMOKE_PASS');await browser.close();
})().catch(e=>{console.error(e);process.exit(1)});
