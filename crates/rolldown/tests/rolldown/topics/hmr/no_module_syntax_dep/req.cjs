const required = require('./required.js');
module.exports = {
  keys: required === undefined ? 'UNDEFINED' : Object.keys(required).sort().join(','),
};
