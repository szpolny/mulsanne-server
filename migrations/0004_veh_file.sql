-- The livery's LMU `.VEH` file name without extension, e.g. `397_25_963`.
-- The app uses it to show the game's render of the car. Empty for laps
-- uploaded before it was sent.
ALTER TABLE laps ADD COLUMN veh_file TEXT NOT NULL DEFAULT '';
