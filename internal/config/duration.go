package config

import "time"

// Duration wraps time.Duration so config files can use plain strings like
// "168h" or "30m" instead of nanosecond integers.
type Duration time.Duration

func (d Duration) AsDuration() time.Duration {
	return time.Duration(d)
}

func (d *Duration) UnmarshalYAML(unmarshal func(interface{}) error) error {
	var raw string
	if err := unmarshal(&raw); err != nil {
		return err
	}
	parsed, err := time.ParseDuration(raw)
	if err != nil {
		return err
	}
	*d = Duration(parsed)
	return nil
}

func (d Duration) MarshalYAML() (interface{}, error) {
	return time.Duration(d).String(), nil
}
