'use strict';
// Synthetic input only. The actual pinned factory computes every policy/result field.
const budgetModule = require(__BUDGET__);
const actual = budgetModule.createInferenceBudget;
budgetModule.createInferenceBudget = options => actual({ ...options, readMeminfo: () => __MEMINFO__ });
