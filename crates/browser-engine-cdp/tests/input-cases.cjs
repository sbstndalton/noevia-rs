'use strict';
module.exports={
 html:()=>`<p id=text>input fixture</p><p id=effect></p><label id=label for=field>Editable</label><input id=field oninput="document.querySelector('#effect').textContent=this.value"><input id=color type=color oninput="document.querySelector('#effect').textContent=this.value"><input id=range type=range oninput="document.querySelector('#effect').textContent=this.value"><input id=number type=number oninput="document.querySelector('#effect').textContent=this.value"><div id=editable contenteditable oninput="document.querySelector('#effect').textContent=this.textContent"></div><select id=select onchange="document.querySelector('#effect').textContent=this.value"><option value=a>First</option><option value=b>Second</option></select>`,
 actions:base=>[
 ['navigate',{type:'navigate',url:base}],
 ['color',{type:'type',selector:'#color',text:'#AABBCC'}],['color-effect',{type:'extract',selector:'#effect'}],
 ['range',{type:'type',selector:'#range',text:'25'}],['range-effect',{type:'extract',selector:'#effect'}],
 ['number-bad',{type:'type',selector:'#number',text:'not-a-number'}],
 ['number',{type:'type',selector:'#number',text:'25.5'}],['number-effect',{type:'extract',selector:'#effect'}],
 ['label-fill',{type:'type',selector:'#label',text:'via label'}],['label-effect',{type:'extract',selector:'#effect'}],
 ['label-clear',{type:'type',selector:'#label',text:''}],['label-clear-effect',{type:'extract',selector:'#effect'}],
 ['editable',{type:'type',selector:'#editable',text:'content editable'}],['editable-effect',{type:'extract',selector:'#effect'}],
 ['select-label',{type:'select',selector:'#select',value:'Second'}],['select-label-effect',{type:'extract',selector:'#effect'}],
 ['bad-target-fill',{type:'type',selector:'#text',text:'wrong'}],
 ['boolean-fill',{type:'type',selector:'#field',text:false}],['boolean-effect',{type:'extract',selector:'#effect'}],
 ['numeric-fill',{type:'type',selector:'#field',text:123}],['numeric-effect',{type:'extract',selector:'#effect'}],
 ],
};
