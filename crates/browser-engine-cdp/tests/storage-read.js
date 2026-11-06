(async()=>{
  const databases=(await indexedDB.databases()).map(v=>v.name).sort();
  const idb=databases.includes('isolation-marker')?await new Promise((resolve,reject)=>{const r=indexedDB.open('isolation-marker');r.onerror=()=>reject(Error('database read failed'));r.onsuccess=()=>{const db=r.result;const q=db.transaction('values').objectStore('values').get('marker');q.onsuccess=()=>{const value=q.result;db.close();resolve(value)};q.onerror=()=>reject(Error('database value failed'));}}):null;
  const cacheNames=(await caches.keys()).sort();const cached={};for(const name of cacheNames){const c=await caches.open(name);const value=await c.match('/'+name);cached[name]=value?await value.text():null;}
  return {cookie:document.cookie,local:localStorage.getItem('marker'),session:sessionStorage.getItem('marker'),databases,idb,cached,workers:(await navigator.serviceWorker.getRegistrations()).map(v=>({scope:v.scope,script:v.active?.scriptURL})).sort((a,b)=>a.scope.localeCompare(b.scope)),permission:(await navigator.permissions.query({name:'geolocation'})).state};
})()
