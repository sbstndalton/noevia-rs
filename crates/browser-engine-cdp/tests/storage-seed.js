(async()=>{
  if(!isSecureContext || !navigator.serviceWorker)throw Error('Positive secure service worker capability required');
  document.cookie='marker=one; Path=/; SameSite=Lax';
  localStorage.setItem('marker','one');sessionStorage.setItem('marker','one');
  const cache=await caches.open('page-marker');await cache.put('/page-marker',new Response('page-one'));
  await new Promise((resolve,reject)=>{const request=indexedDB.open('isolation-marker',1);request.onupgradeneeded=()=>request.result.createObjectStore('values');request.onerror=()=>reject(Error('database unavailable'));request.onsuccess=()=>{const db=request.result;const tx=db.transaction('values','readwrite');tx.objectStore('values').put('one','marker');tx.oncomplete=()=>{db.close();resolve()};tx.onerror=()=>reject(Error('database write failed'));}});
  // Deliberately seed real storage despite the register shim. This is a fixture-only
  // capability probe, not a claim that a blocked registration created an active worker.
  const registration=await ServiceWorkerContainer.prototype.register.call(navigator.serviceWorker,'/sw.js');
  await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('service worker did not activate')),4000);const check=()=>{if(registration.active?.state==='activated'){clearTimeout(timer);resolve()}};registration.installing?.addEventListener('statechange',check);registration.waiting?.addEventListener('statechange',check);check();});
  const worker=await caches.open('worker-marker');const value=await worker.match('/worker-marker');if(!value || await value.text()!=='worker-one')throw Error('Positive worker execution required');
  return true;
})()
