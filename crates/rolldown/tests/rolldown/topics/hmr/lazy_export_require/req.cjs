const required = require('./required.json');
const both = require('./both.json');

module.exports = {
  requiredKeys: Object.keys(required).sort().join(','),
  bothKeys: Object.keys(both).sort().join(','),
};
