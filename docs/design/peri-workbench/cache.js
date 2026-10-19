/* The user requested this board's cache reset. Run once, only for this app. */
window.PeriBoardCache = (() => {
  'use strict';
  function reset(revision){
    if(!revision)return {status:'unused',removed:0};
    const marker='peri-design-reset:'+revision;
    try{
      if(localStorage.getItem(marker))return {status:'already-reset',removed:0};
      const keys=[];for(let i=0;i<localStorage.length;i++){const key=localStorage.key(i);if(key?.startsWith('peri-design-workbench:'))keys.push(key);}
      for(const key of keys)localStorage.removeItem(key);
      localStorage.setItem(marker,JSON.stringify({at:new Date().toISOString(),removed:keys.length}));
      return {status:'reset',removed:keys.length};
    }catch{return {status:'unavailable',removed:0};}
  }
  return {reset};
})();
